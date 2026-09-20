//! Bounded histories. Nothing in a session grows without limit.

use crate::snapshot::Operation;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// How many executed actions a session remembers. Jev sees the last 10; the rest is for the
/// host summary and loop detection.
pub const MAX_ACTION_HISTORY: usize = 50;
/// How many page states a session retains for host reasoning context.
pub const MAX_PAGE_STATES: usize = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionRecord {
    pub step: u32,
    pub operation: Operation,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// The text typed, if any. Held separately from the redacted view.
    #[serde(skip_serializing)]
    pub text: Option<String>,
    /// True when the typed value came from a sensitive key.
    #[serde(default)]
    pub text_sensitive: bool,
    /// Which value-pool key supplied the text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value_key: Option<String>,
    pub confidence: f64,
    pub jev_latency_ms: u64,
    pub browser_latency_ms: u64,
    /// `None` until the post-action observation lands.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_changed: Option<bool>,
    pub url: String,
    pub elapsed_ms: u64,
}

impl ActionRecord {
    /// The only text representation allowed into a Jev request, a log line, or a host reply.
    pub fn text_redacted(&self) -> Option<String> {
        match (&self.text, self.text_sensitive) {
            (Some(_), true) => Some("<redacted>".to_string()),
            (Some(text), false) => Some(text.clone()),
            (None, _) => None,
        }
    }
}

/// A ring buffer that drops the oldest entry once full.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bounded<T> {
    items: VecDeque<T>,
    capacity: usize,
}

impl<T> Bounded<T> {
    pub fn new(capacity: usize) -> Self {
        Self { items: VecDeque::with_capacity(capacity.min(64)), capacity: capacity.max(1) }
    }

    pub fn push(&mut self, item: T) {
        if self.items.len() == self.capacity {
            self.items.pop_front();
        }
        self.items.push_back(item);
    }

    pub fn last_mut(&mut self) -> Option<&mut T> {
        self.items.back_mut()
    }

    pub fn last(&self) -> Option<&T> {
        self.items.back()
    }

    /// Most recent `n`, oldest first.
    pub fn recent(&self, n: usize) -> Vec<&T> {
        let skip = self.items.len().saturating_sub(n);
        self.items.iter().skip(skip).collect()
    }

    pub fn as_slice(&self) -> Vec<&T> {
        self.items.iter().collect()
    }

    pub fn to_vec(&self) -> Vec<T>
    where
        T: Clone,
    {
        self.items.iter().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}
