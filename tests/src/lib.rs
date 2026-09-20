//! Shared scaffolding for the integration tests: fixtures, and a one-line session builder.

use jev_browser_relay_browser::ScriptedBackend;
use jev_browser_relay_core::config::RuntimeConfig;
use jev_browser_relay_core::jev::JevTransport;
use jev_browser_relay_core::session::Session;
use std::sync::Arc;

/// Fixtures are shared with the benchmark harness, so a scenario is described exactly once.
pub const FLIGHTS: &str = include_str!("../../fixtures/flights.json");
pub const DOCS: &str = include_str!("../../fixtures/docs.json");
pub const SIGNUP: &str = include_str!("../../fixtures/signup.json");
pub const AMBIGUOUS: &str = include_str!("../../fixtures/ambiguous.json");
pub const CHECKOUT: &str = include_str!("../../fixtures/checkout.json");

/// A config with everything deterministic: a fixed "today", and budgets big enough that a test
/// only stops for the reason it is testing.
pub fn test_config() -> RuntimeConfig {
    RuntimeConfig {
        today: "2026-09-20".into(),
        default_max_steps: 40,
        default_max_duration_ms: 30_000,
        ..RuntimeConfig::default()
    }
}

pub async fn session_with(
    fixture: &str,
    goal: &str,
    context: Option<serde_json::Value>,
    jev: Arc<dyn JevTransport>,
    config: RuntimeConfig,
) -> Session {
    let backend = ScriptedBackend::from_json(fixture).expect("fixture parses");
    Session::start("ses_test".into(), goal.into(), context, jev, Box::new(backend), config)
        .await
        .expect("session starts")
}
