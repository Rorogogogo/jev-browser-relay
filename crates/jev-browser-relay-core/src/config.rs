//! Runtime knobs. Every threshold that shapes behaviour is here, not scattered through the loop.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    /// TypeSafe model id.
    pub jev_model: String,

    // --- budgets ---
    pub default_max_steps: u32,
    pub default_max_duration_ms: u64,
    /// Hard ceiling across all `run` calls in a session.
    pub max_session_steps: u32,
    pub max_stale_retries: u32,
    pub max_jev_retries: u32,
    pub jev_timeout_ms: u64,

    // --- value resolution ---
    /// Below this, the runtime asks the host instead of typing a guess.
    pub value_confidence_threshold: f64,

    // --- host reasoning escape hatch ---
    pub reasoning_enabled: bool,
    pub reasoning_confidence_threshold: f64,
    /// Minimum gap between the top two targets before the choice counts as ambiguous.
    pub reasoning_margin_threshold: f64,
    /// Unchanged-page actions before asking the host for guidance.
    pub no_change_reasoning_threshold: u32,
    /// Unchanged-page actions before declaring the session blocked.
    pub no_change_blocked_threshold: u32,

    // --- safety ---
    pub safety_enabled: bool,
    pub extra_consequential_phrases: Vec<String>,

    /// Today's date (ISO 8601), injected so date resolution is deterministic and testable.
    pub today: String,

    /// Sessions idle longer than this may be reaped.
    pub session_idle_timeout_ms: u64,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            jev_model: "jev-latest".into(),
            default_max_steps: 30,
            default_max_duration_ms: 60_000,
            max_session_steps: 120,
            max_stale_retries: 5,
            max_jev_retries: 3,
            jev_timeout_ms: 25_000,
            value_confidence_threshold: 0.6,
            reasoning_enabled: true,
            // Deliberately low. Host reasoning is an escape hatch, not a step in normal
            // execution — every trigger here is a round trip the project exists to avoid.
            reasoning_confidence_threshold: 0.35,
            reasoning_margin_threshold: 0.05,
            no_change_reasoning_threshold: 3,
            no_change_blocked_threshold: 5,
            safety_enabled: true,
            extra_consequential_phrases: Vec::new(),
            today: "1970-01-01".into(),
            session_idle_timeout_ms: 30 * 60 * 1000,
        }
    }
}

impl RuntimeConfig {
    /// Overlay environment variables. Only non-secret tuning is env-configurable.
    pub fn from_env() -> Self {
        let mut config = Self::default();
        if let Ok(model) = std::env::var("TYPESAFE_MODEL") {
            if !model.trim().is_empty() {
                config.jev_model = model;
            }
        }
        if let Some(v) = env_u32("JEV_RELAY_MAX_STEPS") {
            config.default_max_steps = v;
        }
        if let Some(v) = env_u64("JEV_RELAY_MAX_DURATION_MS") {
            config.default_max_duration_ms = v;
        }
        if let Some(v) = env_f64("JEV_RELAY_VALUE_THRESHOLD") {
            config.value_confidence_threshold = v.clamp(0.0, 1.0);
        }
        if let Some(v) = env_f64("JEV_RELAY_REASONING_THRESHOLD") {
            config.reasoning_confidence_threshold = v.clamp(0.0, 1.0);
        }
        if std::env::var("JEV_RELAY_DISABLE_SAFETY").as_deref() == Ok("1") {
            config.safety_enabled = false;
        }
        if std::env::var("JEV_RELAY_DISABLE_REASONING").as_deref() == Ok("1") {
            config.reasoning_enabled = false;
        }
        if let Ok(phrases) = std::env::var("JEV_RELAY_CONSEQUENTIAL_PHRASES") {
            config.extra_consequential_phrases =
                phrases.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        }
        config.today = today_iso();
        config
    }
}

fn env_u32(key: &str) -> Option<u32> {
    std::env::var(key).ok()?.trim().parse().ok()
}
fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key).ok()?.trim().parse().ok()
}
fn env_f64(key: &str) -> Option<f64> {
    std::env::var(key).ok()?.trim().parse().ok()
}

/// Today in ISO 8601, computed from the system clock with a proleptic Gregorian conversion.
/// Avoids pulling a date crate in for one string.
pub fn today_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Howard Hinnant's `civil_from_days`, days since 1970-01-01 → (year, month, day).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
