//! A deterministic in-memory browser, driven by a small JSON fixture.
//!
//! This is not a mock that returns canned snapshots — it is a tiny page model with real state:
//! typing changes field values, clicking fires transitions, and the freshness marker is derived
//! from the state so stale-decision handling is exercised for real.
//!
//! It exists so that `cargo test` and the benchmark harness need no Chrome, no daemon, and no
//! API key, while running the *same* loop code as the harness backend.

use async_trait::async_trait;
use jev_browser_relay_core::browser::BrowserBackend;
use jev_browser_relay_core::error::{RelayError, Result};
use jev_browser_relay_core::snapshot::{ActionKind, RawAction, Scroll, Snapshot};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fixture {
    pub start: String,
    pub pages: BTreeMap<String, PageSpec>,
    /// Inject one stale page: after the Nth observation the page silently mutates, so the
    /// runtime must notice at its next freshness check and re-observe.
    #[serde(default)]
    pub stale_after_observation: Option<u32>,

    /// Fail the Nth freshness check outright. Unlike `stale_after_observation`, this can land
    /// *between* a decision and its execution — the window the stale-decision guard exists for,
    /// and the only way to exercise it, since a change noticed before deciding is not a stale
    /// decision at all.
    #[serde(default)]
    pub stale_on_freshness_check: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageSpec {
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub elements: Vec<ElementSpec>,
    /// `element key` → page key. Firing element's click moves the model to that page.
    #[serde(default)]
    pub transitions: BTreeMap<String, String>,
    /// Text appended to the page once every `requires` field is non-empty.
    #[serde(default)]
    pub reveal_text: Option<RevealSpec>,
    #[serde(default)]
    pub scrollable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevealSpec {
    pub requires: Vec<String>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElementSpec {
    /// Stable key used by transitions and by tests.
    pub key: String,
    pub label: String,
    #[serde(default = "default_kind")]
    pub kind: ActionKind,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub value: String,
    /// For `select` elements.
    #[serde(default)]
    pub options: Vec<SelectOptionSpec>,
    /// Only visible once these element keys hold a non-empty value.
    #[serde(default)]
    pub visible_when_filled: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectOptionSpec {
    pub label: String,
    pub value: String,
}

fn default_kind() -> ActionKind {
    ActionKind::Click
}

/// One executed action, recorded so tests can assert on what the loop actually did.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ExecutedAction {
    pub kind: ActionKind,
    pub element_key: String,
    pub label: String,
    pub text: Option<String>,
}

pub struct ScriptedBackend {
    fixture: Fixture,
    current: String,
    /// element key → current value. Global, not per page: a value typed into a search form is
    /// still in effect on the results page, which is what makes verification meaningful.
    values: BTreeMap<String, String>,
    selected: BTreeMap<String, String>,
    scroll_y: f64,
    /// Node ids are assigned per element key and stay stable, like the real in-page cache.
    nodes: BTreeMap<String, i64>,
    next_node: i64,
    history: Vec<String>,

    observations: u32,
    freshness_checks: u32,
    protocol_calls: u32,
    /// Set when the fixture asked for an injected mutation.
    pending_mutation: bool,
    pub executed: Vec<ExecutedAction>,
    closed: bool,
}

impl ScriptedBackend {
    pub fn new(fixture: Fixture) -> Result<Self> {
        if !fixture.pages.contains_key(&fixture.start) {
            return Err(RelayError::Config(format!(
                "fixture start page \"{}\" does not exist",
                fixture.start
            )));
        }
        let current = fixture.start.clone();
        Ok(Self {
            fixture,
            current,
            values: BTreeMap::new(),
            selected: BTreeMap::new(),
            scroll_y: 0.0,
            nodes: BTreeMap::new(),
            next_node: 1,
            history: Vec::new(),
            observations: 0,
            freshness_checks: 0,
            protocol_calls: 0,
            pending_mutation: false,
            executed: Vec::new(),
            closed: false,
        })
    }

    pub fn from_json(raw: &str) -> Result<Self> {
        let fixture: Fixture =
            serde_json::from_str(raw).map_err(|e| RelayError::Config(format!("invalid fixture: {e}")))?;
        Self::new(fixture)
    }

    pub fn current_page(&self) -> &str {
        &self.current
    }

    /// Current value of a field, for assertions.
    pub fn value_of(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    fn page(&self) -> &PageSpec {
        self.fixture.pages.get(&self.current).expect("current page always exists")
    }

    fn node_for(&mut self, key: &str) -> i64 {
        if let Some(node) = self.nodes.get(key) {
            return *node;
        }
        let node = self.next_node;
        self.next_node += 1;
        self.nodes.insert(key.to_string(), node);
        node
    }

    fn page_values(&self) -> BTreeMap<String, String> {
        self.values.clone()
    }

    fn is_visible(&self, element: &ElementSpec) -> bool {
        let values = self.page_values();
        element.visible_when_filled.iter().all(|key| values.get(key).is_some_and(|v| !v.is_empty()))
    }

    fn rendered_text(&self) -> String {
        let page = self.page();
        let mut text = page.text.clone();
        if let Some(reveal) = &page.reveal_text {
            let values = self.page_values();
            if reveal.requires.iter().all(|key| values.get(key).is_some_and(|v| !v.is_empty())) {
                // Substitute the values people actually typed, so verification has something real
                // to find on the "page".
                let mut revealed = reveal.text.clone();
                // A rendered page shows a select's chosen label, so substitute those first.
                for (key, label) in &self.selected {
                    revealed = revealed.replace(&format!("{{{key}}}"), label);
                }
                for (key, value) in &values {
                    revealed = revealed.replace(&format!("{{{key}}}"), value);
                }
                text.push('\n');
                text.push_str(&revealed);
            }
        }
        text
    }

    /// The marker mirrors what the real snapshot script produces: page identity plus every
    /// observable semantic. Any state change invalidates a pending decision.
    fn marker(&self) -> Value {
        json!([
            self.current,
            self.page().url,
            self.scroll_y,
            self.rendered_text(),
            self.page_values(),
            self.selected,
            self.pending_mutation,
        ])
    }

    fn element(&self, key: &str) -> Option<&ElementSpec> {
        self.page().elements.iter().find(|e| e.key == key)
    }

    /// Recover the element key from an action id of the form `e<n>:<key>`.
    fn key_of(action: &RawAction) -> Result<String> {
        action.id.split_once(':').map(|(_, key)| key.to_string()).ok_or_else(|| {
            RelayError::Browser(format!("scripted action id {} has no element key", action.id))
        })
    }
}

#[async_trait]
impl BrowserBackend for ScriptedBackend {
    async fn observe(&mut self) -> Result<Snapshot> {
        if self.closed {
            return Err(RelayError::BrowserDisconnected("the scripted browser is closed".into()));
        }
        self.observations += 1;
        self.protocol_calls += 1;

        // Arm the injected mutation: the next freshness check will fail exactly once.
        if self.fixture.stale_after_observation == Some(self.observations) {
            self.pending_mutation = true;
        }

        let page = self.page().clone();
        let values = self.page_values();
        let selected = self.selected.clone();
        let mut actions = Vec::new();
        let mut guards = BTreeMap::new();

        for (position, element) in page.elements.iter().enumerate() {
            if !self.is_visible(element) {
                continue;
            }
            let node = self.node_for(&element.key);
            let id = format!("e{}:{}", position + 1, element.key);
            let current = values.get(&element.key).cloned().unwrap_or_else(|| element.value.clone());

            guards
                .insert(node.to_string(), json!([node, element.label, current, selected.get(&element.key)]));

            match element.kind {
                ActionKind::Select => {
                    // Each option is its own action with its own id: a chosen target must
                    // resolve to exactly one option, never just to the element.
                    for (ordinal, option) in element.options.iter().enumerate() {
                        actions.push(RawAction {
                            id: format!("e{}o{}:{}", position + 1, ordinal + 1, element.key),
                            kind: ActionKind::Select,
                            label: format!("{} → {}", element.label, option.label),
                            node: Some(node),
                            role: element.role.clone().or_else(|| Some("combobox".into())),
                            value: Some(option.value.clone()),
                            current_value: selected.get(&element.key).cloned(),
                            checked: None,
                            selected: None,
                            expanded: None,
                            delta: None,
                        });
                    }
                }
                kind => {
                    actions.push(RawAction {
                        id: id.clone(),
                        kind,
                        label: element.label.clone(),
                        node: Some(node),
                        role: element.role.clone(),
                        value: Some(current.clone()),
                        current_value: None,
                        checked: None,
                        selected: None,
                        expanded: None,
                        delta: None,
                    });
                }
            }
        }

        if page.scrollable {
            actions.push(RawAction {
                id: "scroll_down".into(),
                kind: ActionKind::Scroll,
                label: "Scroll down".into(),
                node: None,
                role: None,
                value: None,
                current_value: None,
                checked: None,
                selected: None,
                expanded: None,
                delta: Some(560),
            });
            if self.scroll_y > 0.0 {
                actions.push(RawAction {
                    id: "scroll_up".into(),
                    kind: ActionKind::Scroll,
                    label: "Scroll up".into(),
                    node: None,
                    role: None,
                    value: None,
                    current_value: None,
                    checked: None,
                    selected: None,
                    expanded: None,
                    delta: Some(-560),
                });
            }
        }

        actions.push(RawAction {
            id: "wait".into(),
            kind: ActionKind::Wait,
            label: "Wait for the page to update".into(),
            node: None,
            role: None,
            value: None,
            current_value: None,
            checked: None,
            selected: None,
            expanded: None,
            delta: None,
        });

        Ok(Snapshot {
            url: page.url.clone(),
            title: page.title.clone(),
            text: self.rendered_text(),
            scroll: Scroll { y: self.scroll_y, height: 2000.0 },
            actions,
            marker: self.marker(),
            page_key: json!([self.current, self.page_values()]),
            guards,
            omitted_actions: 0,
            can_go_back: !self.history.is_empty(),
            generation: 0,
            screenshot: None,
        })
    }

    async fn is_fresh(&mut self, snapshot: &Snapshot, _action: Option<&RawAction>) -> Result<bool> {
        self.protocol_calls += 1;
        self.freshness_checks += 1;
        if self.fixture.stale_on_freshness_check == Some(self.freshness_checks) {
            return Ok(false);
        }
        if self.pending_mutation {
            // Consume the injection: the runtime should re-observe and then proceed normally.
            self.pending_mutation = false;
            return Ok(false);
        }
        Ok(self.marker() == snapshot.marker)
    }

    async fn execute(&mut self, action: &RawAction, _snapshot: &Snapshot, text: Option<&str>) -> Result<()> {
        if self.closed {
            return Err(RelayError::BrowserDisconnected("the scripted browser is closed".into()));
        }
        self.protocol_calls += 1;

        match action.kind {
            ActionKind::Wait => {
                self.executed.push(ExecutedAction {
                    kind: ActionKind::Wait,
                    element_key: "wait".into(),
                    label: action.label.clone(),
                    text: None,
                });
                return Ok(());
            }
            ActionKind::Scroll => {
                self.scroll_y = (self.scroll_y + action.delta.unwrap_or(560) as f64).max(0.0);
                self.executed.push(ExecutedAction {
                    kind: ActionKind::Scroll,
                    element_key: action.id.clone(),
                    label: action.label.clone(),
                    text: None,
                });
                return Ok(());
            }
            _ => {}
        }

        let key = Self::key_of(action)?;
        let element = self
            .element(&key)
            .ok_or_else(|| RelayError::StalePage(format!("element {key} is no longer on the page")))?
            .clone();
        if !self.is_visible(&element) {
            return Err(RelayError::StalePage(format!("element {key} is not visible")));
        }

        self.executed.push(ExecutedAction {
            kind: action.kind,
            element_key: key.clone(),
            label: element.label.clone(),
            text: text.map(str::to_string),
        });

        match action.kind {
            ActionKind::Fill => {
                let value =
                    text.ok_or_else(|| RelayError::Browser("fill reached the executor with no text".into()))?;
                self.values.insert(key.clone(), value.to_string());
            }
            ActionKind::Select => {
                let value = action.value.clone().unwrap_or_default();
                if !element.options.iter().any(|o| o.value == value) {
                    return Err(RelayError::StalePage(format!("option {value} is not offered by {key}")));
                }
                let label = element
                    .options
                    .iter()
                    .find(|o| o.value == value)
                    .map(|o| o.label.clone())
                    .unwrap_or(value.clone());
                self.selected.insert(key.clone(), label);
                self.values.insert(key.clone(), value);
            }
            ActionKind::Click => {
                if let Some(next) = self.page().transitions.get(&key).cloned() {
                    if !self.fixture.pages.contains_key(&next) {
                        return Err(RelayError::Navigation(format!(
                            "fixture transition targets missing page {next}"
                        )));
                    }
                    self.history.push(self.current.clone());
                    self.current = next;
                    self.scroll_y = 0.0;
                } else {
                    // A click with no transition still marks the control, which is what lets a
                    // fixture model checkboxes and tabs.
                    self.values.insert(key.clone(), "clicked".into());
                }
            }
            _ => unreachable!("handled above"),
        }
        Ok(())
    }

    async fn settle(&mut self, _action: &RawAction) -> Result<u64> {
        Ok(0)
    }

    async fn back(&mut self) -> Result<()> {
        self.protocol_calls += 1;
        self.current = self.history.pop().ok_or_else(|| RelayError::Navigation("no previous page".into()))?;
        self.scroll_y = 0.0;
        Ok(())
    }

    fn protocol_calls(&self) -> u32 {
        self.protocol_calls
    }

    async fn close(&mut self) -> Result<()> {
        self.closed = true;
        Ok(())
    }

    fn describe(&self) -> String {
        format!("scripted:{}", self.current)
    }
}
