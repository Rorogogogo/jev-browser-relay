//! Independent DONE verification.
//!
//! Jev choosing DONE is a claim, not evidence. The runtime re-observes the page and runs
//! deterministic checks against the task's own facts. When those checks cannot settle it, the
//! session still reports `done` but flags `host_verification_required` and hands back the final
//! page state — the runtime never asserts success on the policy model's say-so alone.

use crate::snapshot::Snapshot;
use crate::value_pool::{normalize, ValuePool};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Every check that could run, passed.
    Verified,
    /// At least one check failed outright.
    Failed,
    /// Nothing decisive could be checked deterministically. The host must look.
    Inconclusive,
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: String,
    pub passed: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct VerificationReport {
    pub verdict: Verdict,
    pub checks: Vec<Check>,
    /// True whenever the host should confirm the outcome itself.
    pub host_verification_required: bool,
    pub final_url: String,
    pub final_title: String,
    /// Trimmed page text, so the host can judge without another browser call.
    pub page_excerpt: String,
}

/// Run the deterministic checks.
///
/// - every non-sensitive task value should be visible on the page or sitting in a field
/// - the page should have moved on from where the task started
/// - the page should not be showing an obvious error or empty-result state
pub fn verify(snapshot: &Snapshot, pool: &ValuePool, initial_url: &str, goal: &str) -> VerificationReport {
    let mut checks = Vec::new();

    let haystack = normalize(&format!(
        "{} {} {} {}",
        snapshot.title,
        snapshot.url,
        snapshot.text,
        snapshot
            .actions
            .iter()
            .map(|a| format!("{} {}", a.label, a.value.clone().unwrap_or_default()))
            .collect::<Vec<_>>()
            .join(" ")
    ));

    let squashed_haystack: String = haystack.chars().filter(char::is_ascii_alphanumeric).collect();
    let assertable = pool.assertable();
    let mut checked_values = 0;
    for entry in &assertable {
        // Only check values that are plausibly rendered as text. A long free-text blob or a
        // boolean tells us nothing about success.
        let value = normalize(&entry.value);
        if value.is_empty() || value.len() > 60 || matches!(value.as_str(), "true" | "false") {
            continue;
        }
        checked_values += 1;
        // The same fact often reaches the page in a different surface form: "one-way" rendered
        // as "One way", an ISO date rendered as "10 Oct 2026". Check those too, or a correct
        // run gets reported as unverified.
        let present = haystack.contains(&value)
            || date_variants(&entry.value).iter().any(|v| haystack.contains(v))
            || squashed_match(&squashed_haystack, &value);
        checks.push(Check {
            name: format!("value_present:{}", entry.key),
            passed: present,
            detail: if present {
                format!("\"{}\" is visible on the final page", entry.redacted())
            } else {
                format!("\"{}\" was not found on the final page", entry.redacted())
            },
        });
    }

    let moved = snapshot.url != initial_url;
    checks.push(Check {
        name: "navigation_progressed".into(),
        passed: moved,
        detail: if moved {
            format!("url changed from {initial_url} to {}", snapshot.url)
        } else {
            "url is unchanged since the task started".into()
        },
    });

    let error_state =
        ["page not found", "404", "something went wrong", "no results found", "try again later"]
            .iter()
            .find(|needle| haystack.contains(&normalize(needle)));
    checks.push(Check {
        name: "no_error_state".into(),
        passed: error_state.is_none(),
        detail: match error_state {
            Some(found) => format!("page shows \"{found}\""),
            None => "no obvious error or empty-result text".into(),
        },
    });

    // Goal keywords are weak evidence, so they inform the verdict only when nothing stronger ran.
    let goal_terms: Vec<String> =
        normalize(goal).split_whitespace().filter(|w| w.len() > 4).map(str::to_string).collect();
    let goal_hits = goal_terms.iter().filter(|t| haystack.contains(*t)).count();
    if !goal_terms.is_empty() {
        let passed = goal_hits * 2 >= goal_terms.len();
        checks.push(Check {
            name: "goal_terms_visible".into(),
            passed,
            detail: format!("{goal_hits}/{} distinctive goal terms visible", goal_terms.len()),
        });
    }

    let failures = checks.iter().filter(|c| !c.passed).count();
    // Without any task values to look for, the remaining checks are too weak to call a success.
    let verdict = if failures > 0 {
        Verdict::Failed
    } else if checked_values == 0 {
        Verdict::Inconclusive
    } else {
        Verdict::Verified
    };

    VerificationReport {
        verdict,
        host_verification_required: verdict != Verdict::Verified,
        checks,
        final_url: snapshot.url.clone(),
        final_title: snapshot.title.clone(),
        page_excerpt: snapshot.text.chars().take(1500).collect(),
    }
}

/// Compare with all separators removed, so "one-way" matches "One way" and "oneway".
/// Short values are excluded: a two- or three-character needle matches almost anything once
/// spacing is discarded.
fn squashed_match(squashed_haystack: &str, value: &str) -> bool {
    let needle: String = value.chars().filter(char::is_ascii_alphanumeric).collect();
    needle.len() >= 4 && squashed_haystack.contains(&needle)
}

/// Alternative renderings of an ISO date, so `2026-10-10` still verifies against "10 Oct 2026".
fn date_variants(value: &str) -> Vec<String> {
    const MONTHS: [&str; 12] = [
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ];
    let parts: Vec<&str> = value.split('-').collect();
    if parts.len() != 3 {
        return Vec::new();
    }
    let (Ok(year), Ok(month), Ok(day)) =
        (parts[0].parse::<u32>(), parts[1].parse::<usize>(), parts[2].parse::<u32>())
    else {
        return Vec::new();
    };
    if !(1..=12).contains(&month) {
        return Vec::new();
    }
    let name = MONTHS[month - 1];
    let short = &name[..3];
    vec![
        normalize(&format!("{day} {name} {year}")),
        normalize(&format!("{day} {short} {year}")),
        normalize(&format!("{name} {day} {year}")),
        normalize(&format!("{short} {day} {year}")),
        normalize(&format!("{day} {short}")),
        normalize(&format!("{short} {day}")),
    ]
}
