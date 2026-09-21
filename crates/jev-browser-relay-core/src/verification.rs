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
    /// Whether failing this check is on its own enough to call the task failed.
    ///
    /// Not every signal deserves a veto. A live Google Flights run set the right route, the
    /// right date and the right cabin, had all six of those values visible on the results page —
    /// and was reported as failed because fewer than half the words in the goal appeared. Words
    /// like "departing", "matching" and "stop" are instructions to the agent, not content that
    /// any results page would ever show. One weak heuristic must not overrule the direct
    /// evidence.
    pub decisive: bool,
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

    // `haystack` is already normalized, so diacritics are folded; squashing only removes the
    // separators that remain.
    let squashed_haystack: String = haystack.chars().filter(|c| c.is_alphanumeric()).collect();
    let assertable = pool.assertable();
    let mut checked_values = 0;
    let mut visible_values = 0;
    for entry in &assertable {
        // Only check values that are plausibly rendered as text. A long free-text blob or a
        // boolean tells us nothing about success.
        let value = normalize(&entry.value);
        if value.is_empty() || value.len() > 60 || matches!(value.as_str(), "true" | "false") {
            continue;
        }
        checked_values += 1;
        let present = value_on_page(&haystack, &squashed_haystack, &entry.value, &value);
        if present {
            visible_values += 1;
        }
        checks.push(Check {
            name: format!("value_present:{}", entry.key),
            passed: present,
            // Individually informational. A task value may simply not be rendered on the page
            // that proves success — "language: English" is an input to the search, not
            // something an article displays. Making each absence decisive would mean the more
            // context the host helpfully supplies, the more likely a correct run is reported as
            // failed, which is exactly backwards. What matters is the aggregate below.
            decisive: false,
            detail: if present {
                format!("\"{}\" is visible on the final page", entry.redacted())
            } else {
                format!("\"{}\" was not found on the final page", entry.redacted())
            },
        });
    }

    // The aggregate is the decisive one, and it is deliberately all-or-nothing.
    //
    // Two weaker rules were tried against live runs and both are wrong. Failing on *any* missing
    // value punishes a host for supplying context: `language: English` steers a search and no
    // article page displays it, so a correct run got reported as failed. Passing on *any* present
    // value is worse in the other direction: a flights run that searched the route but never
    // applied the requested Nonstop filter had six of seven values on the page and was reported
    // verified, which is the runtime asserting a success it had evidence against.
    //
    // The honest mapping is three-way, and it falls out of `decisive` plus `checked_values`:
    // everything present is Verified, nothing present is Failed, and anything in between is
    // Inconclusive — the runtime did the work and cannot settle the question, so the host looks.
    // A partial result names the missing value, so that check is cheap to resolve.
    if checked_values > 0 {
        checks.push(Check {
            name: "task_values_visible".into(),
            passed: visible_values > 0,
            decisive: true,
            detail: format!("{visible_values}/{checked_values} task values visible on the final page"),
        });
    }

    let moved = snapshot.url != initial_url;
    checks.push(Check {
        name: "navigation_progressed".into(),
        passed: moved,
        decisive: true,
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
        decisive: true,
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
            // Informational. A goal is phrased as an instruction, so much of its vocabulary
            // describes what to do rather than what the finished page will say.
            decisive: checked_values == 0,
            detail: format!("{goal_hits}/{} distinctive goal terms visible", goal_terms.len()),
        });
    }

    let decisive_failures = checks.iter().filter(|c| c.decisive && !c.passed).count();
    debug_assert!(visible_values <= checked_values);
    let verdict = if decisive_failures > 0 {
        Verdict::Failed
    } else if checked_values == 0 || visible_values < checked_values {
        // Either nothing could be checked, or only some of it landed. Both mean the same thing:
        // the runtime cannot settle this, and saying "verified" would be a claim it cannot back.
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

/// Is this task value evidenced on the page?
///
/// Three progressively looser tests, because a value reaches a page in more shapes than it was
/// written in:
///
/// 1. the phrase verbatim
/// 2. an ISO date rendered as prose — `2026-10-10` as "10 Oct 2026"
/// 3. separators discarded — "one-way" as "One way"
/// 4. for a multi-word value, every word present somewhere
///
/// The last is what handles possessives and inserted particles: the goal says "Godel
/// incompleteness theorems" and the article is titled "Gödel's incompleteness theorems", where
/// the apostrophe-s breaks contiguity without changing the meaning at all.
fn value_on_page(haystack: &str, squashed_haystack: &str, raw: &str, normalized: &str) -> bool {
    if haystack.contains(normalized) {
        return true;
    }
    if date_variants(raw).iter().any(|v| haystack.contains(v)) {
        return true;
    }
    if squashed_match(squashed_haystack, raw) {
        return true;
    }
    // Every word present, each long enough not to match by accident.
    let words: Vec<&str> = normalized.split_whitespace().filter(|w| w.len() >= 3).collect();
    words.len() >= 2 && words.iter().all(|word| haystack.contains(word))
}

/// Compare with all separators removed, so "one-way" matches "One way" and "oneway".
/// Short values are excluded: a two- or three-character needle matches almost anything once
/// spacing is discarded.
fn squashed_match(squashed_haystack: &str, value: &str) -> bool {
    let needle: String = normalize(value).chars().filter(|c| c.is_alphanumeric()).collect();
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
