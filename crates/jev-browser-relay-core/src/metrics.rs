//! Performance is a first-class feature, so the numbers are part of the product surface.
//!
//! Latency is bucketed by *cause* — Jev, browser, waiting, verification — because the whole
//! point of the project is knowing where the time and the round trips actually went.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Metrics {
    pub total_task_ms: u64,

    pub jev_requests: u32,
    pub jev_latency_total_ms: u64,
    /// Every Jev latency sample, for median/p95. Bounded by the step budget.
    #[serde(skip)]
    jev_latencies: Vec<u64>,

    /// The number this project exists to reduce: how many times the host model had to think.
    pub host_round_trips: u32,
    pub host_input_requests: u32,
    pub host_reasoning_requests: u32,
    pub host_confirmations: u32,

    pub browser_actions: u32,
    /// Individual CDP calls — the layer beneath browser actions.
    pub browser_protocol_calls: u32,
    pub snapshots: u32,
    pub browser_latency_total_ms: u64,

    pub stale_decisions: u32,
    pub retries: u32,
    pub wait_ms: u64,
    pub verification_ms: u64,

    /// Text values resolved locally — each one is a host round trip that did not happen.
    pub values_resolved_locally: u32,
}

impl Metrics {
    pub fn record_jev(&mut self, latency_ms: u64) {
        self.jev_requests += 1;
        self.jev_latency_total_ms += latency_ms;
        self.jev_latencies.push(latency_ms);
    }

    pub fn record_browser_action(&mut self, latency_ms: u64) {
        self.browser_actions += 1;
        self.browser_latency_total_ms += latency_ms;
    }

    pub fn record_snapshot(&mut self, latency_ms: u64) {
        self.snapshots += 1;
        self.browser_latency_total_ms += latency_ms;
    }

    pub fn jev_latency_median_ms(&self) -> u64 {
        percentile(&self.jev_latencies, 50.0)
    }

    pub fn jev_latency_p95_ms(&self) -> u64 {
        percentile(&self.jev_latencies, 95.0)
    }

    /// The full report returned by `jev_browser_stop` and written by the benchmark harness.
    pub fn report(&self) -> serde_json::Value {
        serde_json::json!({
            "total_task_ms": self.total_task_ms,
            "jev_requests": self.jev_requests,
            "jev_latency_total_ms": self.jev_latency_total_ms,
            "jev_latency_median_ms": self.jev_latency_median_ms(),
            "jev_latency_p95_ms": self.jev_latency_p95_ms(),
            "host_round_trips": self.host_round_trips,
            "host_input_requests": self.host_input_requests,
            "host_reasoning_requests": self.host_reasoning_requests,
            "host_confirmations": self.host_confirmations,
            "browser_actions": self.browser_actions,
            "browser_protocol_calls": self.browser_protocol_calls,
            "snapshots": self.snapshots,
            "browser_latency_total_ms": self.browser_latency_total_ms,
            "stale_decisions": self.stale_decisions,
            "retries": self.retries,
            "wait_ms": self.wait_ms,
            "verification_ms": self.verification_ms,
            "values_resolved_locally": self.values_resolved_locally,
            "actions_per_host_round_trip": self.actions_per_host_round_trip(),
        })
    }

    /// The headline ratio: browser actions executed per host-model turn. Higher is the goal.
    pub fn actions_per_host_round_trip(&self) -> f64 {
        if self.host_round_trips == 0 {
            return self.browser_actions as f64;
        }
        self.browser_actions as f64 / self.host_round_trips as f64
    }
}

fn percentile(samples: &[u64], p: f64) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    // Nearest-rank.
    let rank = ((p / 100.0) * sorted.len() as f64).ceil().max(1.0) as usize;
    sorted[rank.min(sorted.len()) - 1]
}
