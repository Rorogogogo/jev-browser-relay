//! Deterministic resolution of TYPE_TEXT values.
//!
//! Upstream jev-ultrafast calls a second LLM every time a field needs text. That is the one
//! required key this project removes, so the runtime resolves values itself, in order:
//!
//! 1. structured task context supplied at `start`
//! 2. values the host supplied earlier in this session
//! 3. deterministic session facts (current url, page title, today)
//! 4. semantic aliases over a normalized key space
//!
//! Matching is deliberately a small explainable scorer, not local NLP. When it is not confident
//! the loop pauses with `NEEDS_INPUT` rather than typing a guess — one cheap host round trip,
//! and the answer is cached under its semantic key so every later field with the same meaning
//! resolves for free.

use crate::snapshot::FieldDescription;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Where a value came from. Drives confidence and whether it may be cached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueSource {
    /// Supplied in the `context` object at `start`.
    InitialContext,
    /// Supplied by the host in reply to `NEEDS_INPUT`.
    HostInput,
    /// Read off the page.
    PageState,
    /// Computed by the runtime (today's date, current host, …).
    Derived,
}

impl ValueSource {
    fn base_confidence(self) -> f64 {
        match self {
            Self::InitialContext => 1.0,
            Self::HostInput => 1.0,
            Self::PageState => 0.7,
            Self::Derived => 0.6,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValueEntry {
    /// Normalized key, e.g. `origin`.
    pub key: String,
    /// The key as the host originally wrote it.
    pub original_key: String,
    #[serde(skip_serializing)]
    pub value: String,
    /// Optional human description of what this value means.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meaning: Option<String>,
    pub source: ValueSource,
    pub confidence: f64,
    /// How many times this value has been typed.
    pub uses: u32,
    /// Sensitive values are never logged, never echoed to the host, and never cached from
    /// page state.
    pub sensitive: bool,
}

impl ValueEntry {
    /// A log- and host-safe rendering.
    pub fn redacted(&self) -> String {
        if self.sensitive {
            "<redacted>".to_string()
        } else {
            self.value.clone()
        }
    }
}

/// How a field's text was resolved. Returned so the host can audit, and so traces explain
/// themselves.
#[derive(Debug, Clone, Serialize)]
pub struct Resolution {
    pub key: String,
    #[serde(skip_serializing)]
    pub value: String,
    pub score: f64,
    pub source: ValueSource,
    pub sensitive: bool,
    /// Why this key matched, for traces.
    pub rationale: String,
}

impl Resolution {
    pub fn redacted_value(&self) -> String {
        if self.sensitive {
            "<redacted>".to_string()
        } else {
            self.value.clone()
        }
    }
}

/// Semantic alias groups. The canonical name comes first. A field label and a context key that
/// land in the same group are treated as the same meaning.
///
/// This is intentionally a short, auditable table covering the common web-form vocabulary.
/// Unknown keys still resolve through normalization and token overlap; when nothing scores well
/// enough the runtime asks the host instead of guessing.
const ALIAS_GROUPS: &[&[&str]] = &[
    &[
        "origin",
        "from",
        "from location",
        "departure city",
        "departure airport",
        "leaving from",
        "where from",
        "flying from",
        "source",
        "pick up location",
        "start",
        "start location",
        "depart from",
    ],
    &[
        "destination",
        "to",
        "to location",
        "arrival city",
        "arrival airport",
        "going to",
        "where to",
        "flying to",
        "target",
        "drop off location",
        "end location",
        "arrive at",
    ],
    // A bare "date" belongs here, not with the return date: unqualified, it means the primary
    // outbound date. Because the two are distinct groups, a `date` value can never be typed
    // into a "Return date" field.
    &[
        "departure date",
        "depart",
        "departure",
        "start date",
        "outbound date",
        "leaving on",
        "check in",
        "check in date",
        "from date",
        "when",
        "date",
        "travel date",
        "flight date",
    ],
    &[
        "return date",
        "return",
        "end date",
        "inbound date",
        "coming back",
        "check out",
        "check out date",
        "to date",
    ],
    &[
        "query",
        "search",
        "search query",
        "search term",
        "keyword",
        "keywords",
        "find",
        "q",
        "search for",
        "look for",
    ],
    &["email", "email address", "e mail", "your email", "work email"],
    &["first name", "given name", "forename"],
    &["last name", "surname", "family name"],
    &["full name", "name", "your name", "contact name"],
    &["phone", "phone number", "telephone", "mobile", "mobile number", "contact number"],
    &["address", "street address", "address line 1", "street"],
    &["city", "town", "suburb"],
    &["state", "province", "region"],
    &["postcode", "postal code", "zip", "zip code", "post code"],
    &["country", "nation"],
    &["company", "organisation", "organization", "employer", "business name"],
    &["username", "user name", "login", "user id", "handle"],
    &["password", "pass word", "passphrase"],
    &["message", "comment", "comments", "note", "notes", "description", "details", "body"],
    &["subject", "title", "headline", "summary"],
    &["quantity", "qty", "amount", "number of", "count"],
    &["adults", "number of adults", "passengers", "travellers", "travelers", "guests"],
    &["price", "budget", "max price", "maximum price"],
];

/// Keys whose values must never be logged or echoed, regardless of how they were supplied.
const SENSITIVE_KEYS: &[&str] = &[
    "password",
    "passphrase",
    "secret",
    "token",
    "api key",
    "apikey",
    "pin",
    "otp",
    "one time code",
    "verification code",
    "security code",
    "cvv",
    "cvc",
    "card number",
    "credit card",
    "ssn",
    "social security",
    "tax file number",
    "passport number",
    "bank account",
    "routing number",
    "private key",
    "session cookie",
    "auth",
];

/// Fold a Latin letter carrying a diacritic to its plain form.
///
/// A person writes "Godel", "Zurich" or "Sao Paulo"; the page renders "Gödel", "Zürich",
/// "São Paulo". Without folding, a task that genuinely succeeded fails verification because the
/// value it typed is "not on the page", and a context key never matches its own field. Accented
/// proper nouns are not an edge case in browser work — they are most of the world's city names.
fn fold_diacritic(c: char) -> Option<&'static str> {
    Some(match c {
        'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' | 'ā' | 'ă' | 'ą' => "a",
        'ç' | 'ć' | 'č' => "c",
        'ď' | 'đ' | 'ð' => "d",
        'é' | 'è' | 'ê' | 'ë' | 'ē' | 'ė' | 'ę' | 'ě' => "e",
        'ğ' => "g",
        'í' | 'ì' | 'î' | 'ï' | 'ī' | 'į' | 'ı' => "i",
        'ł' => "l",
        'ñ' | 'ń' | 'ň' => "n",
        'ó' | 'ò' | 'ô' | 'ö' | 'õ' | 'ø' | 'ō' | 'ő' => "o",
        'ř' => "r",
        'ś' | 'š' | 'ş' => "s",
        'ť' | 'ţ' => "t",
        'ú' | 'ù' | 'û' | 'ü' | 'ū' | 'ů' | 'ű' => "u",
        'ý' | 'ÿ' => "y",
        'ž' | 'ź' | 'ż' => "z",
        'þ' => "th",
        'æ' => "ae",
        'œ' => "oe",
        'ß' => "ss",
        _ => return None,
    })
}

/// Lowercase, fold diacritics, strip punctuation, collapse whitespace, drop filler words.
pub fn normalize(raw: &str) -> String {
    // "type" is deliberately absent: it carries meaning in keys like `trip_type` and
    // `card_type`, and stripping it silently merges distinct fields.
    const FILLER: &[&str] = &[
        "the", "a", "an", "your", "please", "enter", "select", "choose", "input", "field", "optional",
        "required",
    ];
    let mut cleaned = String::with_capacity(raw.len());
    // Unicode-aware lowercasing, so 'Ö' becomes 'ö' and then folds to 'o'.
    for c in raw.to_lowercase().chars() {
        match fold_diacritic(c) {
            Some(folded) => cleaned.push_str(folded),
            None if c.is_alphanumeric() => cleaned.push(c),
            None => cleaned.push(' '),
        }
    }
    cleaned.split_whitespace().filter(|w| !FILLER.contains(w)).collect::<Vec<_>>().join(" ")
}

/// Tokens present in `longer` but not in `shorter`.
fn extra_tokens(longer: &[String], shorter: &[String]) -> Vec<String> {
    longer.iter().filter(|t| !shorter.contains(t)).cloned().collect()
}

fn tokens(raw: &str) -> Vec<String> {
    normalize(raw).split_whitespace().map(str::to_string).collect()
}

/// The alias group a normalized phrase belongs to, if any.
fn alias_group(normalized: &str) -> Option<usize> {
    ALIAS_GROUPS.iter().position(|group| group.iter().any(|alias| normalize(alias) == normalized))
}

/// Alias lookup that tolerates a qualifier around a known phrase, so a namespaced context key
/// like `traveller.last_name` reaches the `surname` group and a label like "Search Wikipedia"
/// reaches the `query` group.
///
/// Trimming is guarded rather than free. Any contiguous sub-phrase may match, **but only when
/// every token dropped to reach it is itself semantically inert** — not a member of any alias
/// group. That distinction is what separates the two cases:
///
/// - "search wikipedia" → "search" (query), dropping "wikipedia", which means nothing here. Safe.
/// - "departure city" → "departure" (departure *date*), dropping "city", which is a known
///   meaning of its own. Trimming it changed what the phrase refers to, so it is refused.
///
/// An earlier version trimmed only leading tokens, on the theory that English noun phrases are
/// head-final. That holds for "traveller last name" and fails immediately for "Search Wikipedia",
/// which is a verb phrase. The inertness guard covers both without needing to know which.
fn alias_group_relaxed(normalized: &str) -> Option<(usize, bool)> {
    if let Some(group) = alias_group(normalized) {
        return Some((group, true));
    }
    let tokens: Vec<&str> = normalized.split_whitespace().collect();
    if tokens.len() < 2 {
        return None;
    }
    // Longest sub-phrase first, so "last name" is preferred over "name".
    for length in (1..tokens.len()).rev() {
        for start in 0..=(tokens.len() - length) {
            let Some(group) = alias_group(&tokens[start..start + length].join(" ")) else { continue };
            let dropped_carries_meaning = tokens
                .iter()
                .enumerate()
                .filter(|(index, _)| *index < start || *index >= start + length)
                .any(|(_, token)| alias_group(token).is_some());
            if !dropped_carries_meaning {
                return Some((group, false));
            }
        }
    }
    None
}

fn is_sensitive_key(normalized: &str) -> bool {
    SENSITIVE_KEYS.iter().any(|s| {
        let s = normalize(s);
        normalized == s || normalized.contains(&s)
    })
}

/// Similarity in `[0,1]` between a context key and a field label, with the rationale.
fn similarity(key: &str, label: &str) -> (f64, &'static str) {
    if key.is_empty() || label.is_empty() {
        return (0.0, "empty");
    }
    if key == label {
        return (1.0, "exact key match");
    }
    if let (Some((a, a_exact)), Some((b, b_exact))) = (alias_group_relaxed(key), alias_group_relaxed(label)) {
        if a == b {
            let exact = a_exact && b_exact;
            return if exact {
                (0.92, "semantic alias group")
            } else {
                (0.85, "semantic alias group via suffix")
            };
        }
        // Both sides are known vocabulary but different meanings. Do not fall through to
        // token overlap: "departure date" vs "return date" share a token and must not match.
        return (0.0, "distinct alias groups");
    }

    let key_tokens = tokens(key);
    let label_tokens = tokens(label);
    if key_tokens.is_empty() || label_tokens.is_empty() {
        return (0.0, "no tokens");
    }
    // One phrase fully contained in the other, e.g. "email" in "work email".
    //
    // Guarded by the same inertness rule as alias trimming, and for the same reason: the extra
    // words have to be noise. "work" in "work email" is noise, so the match holds. "country" in
    // "phone country code" is a meaning of its own, and ignoring it would put a phone number in
    // a dialling-code field — a bare containment check happily does exactly that.
    let contained = if key_tokens.iter().all(|t| label_tokens.contains(t)) {
        Some(extra_tokens(&label_tokens, &key_tokens))
    } else if label_tokens.iter().all(|t| key_tokens.contains(t)) {
        Some(extra_tokens(&key_tokens, &label_tokens))
    } else {
        None
    };
    if let Some(extra) = contained {
        if extra.iter().all(|token| alias_group(token).is_none()) {
            return (0.8, "token containment");
        }
        return (0.0, "containment blocked by a meaningful extra word");
    }
    let shared = key_tokens.iter().filter(|t| label_tokens.contains(t)).count() as f64;
    let union = (key_tokens.len() + label_tokens.len()) as f64 - shared;
    if union <= 0.0 {
        return (0.0, "no overlap");
    }
    (0.7 * (shared / union), "token overlap")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValuePool {
    entries: BTreeMap<String, ValueEntry>,
    /// Below this, the runtime asks the host rather than typing a guess.
    pub threshold: f64,
}

impl Default for ValuePool {
    fn default() -> Self {
        Self { entries: BTreeMap::new(), threshold: 0.6 }
    }
}

impl ValuePool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Load the schema-flexible `context` object from `jev_browser_start`.
    ///
    /// Nested objects are flattened with dotted keys; arrays are joined. No field names are
    /// hard-coded — whatever the host supplies becomes a semantic key.
    pub fn load_context(&mut self, context: &serde_json::Value) {
        self.load_value(context, "");
    }

    fn load_value(&mut self, value: &serde_json::Value, prefix: &str) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, child) in map {
                    let path = if prefix.is_empty() { key.clone() } else { format!("{prefix}.{key}") };
                    self.load_value(child, &path);
                }
            }
            serde_json::Value::Array(items) => {
                let joined: Vec<String> = items.iter().filter_map(scalar_to_string).collect();
                if !joined.is_empty() && !prefix.is_empty() {
                    self.insert(prefix, &joined.join(", "), ValueSource::InitialContext, None);
                }
            }
            other => {
                if let (Some(text), false) = (scalar_to_string(other), prefix.is_empty()) {
                    self.insert(prefix, &text, ValueSource::InitialContext, None);
                }
            }
        }
    }

    /// Add or replace a value. The last write for a key wins, which is what makes a host
    /// correction stick.
    pub fn insert(&mut self, key: &str, value: &str, source: ValueSource, meaning: Option<String>) {
        let normalized = normalize(key);
        if normalized.is_empty() || value.is_empty() {
            return;
        }
        let sensitive = is_sensitive_key(&normalized);
        let existing_uses = self.entries.get(&normalized).map(|e| e.uses).unwrap_or(0);
        self.entries.insert(
            normalized.clone(),
            ValueEntry {
                key: normalized,
                original_key: key.to_string(),
                value: value.to_string(),
                meaning,
                source,
                confidence: source.base_confidence(),
                uses: existing_uses,
                sensitive,
            },
        );
    }

    /// Deterministic session facts, refreshed each observation.
    pub fn load_session_facts(&mut self, url: &str, title: &str, today: &str) {
        self.insert("current url", url, ValueSource::Derived, Some("URL of the current page".into()));
        if !title.is_empty() {
            self.insert("current page title", title, ValueSource::Derived, None);
        }
        self.insert("today", today, ValueSource::Derived, Some("Today's date, ISO 8601".into()));
    }

    /// Best match for a field, if any candidate clears the threshold.
    ///
    /// The field's label, role and placeholder are all considered; the strongest signal wins.
    pub fn resolve(&self, field: &FieldDescription) -> Option<Resolution> {
        let label = normalize(&field.label);
        if label.is_empty() {
            return None;
        }

        let mut best: Option<Resolution> = None;
        for entry in self.entries.values() {
            // A derived fact is a fallback, never a confident answer for a form field.
            let penalty = if entry.source == ValueSource::Derived { 0.15 } else { 0.0 };
            let (raw_score, rationale) = similarity(&entry.key, &label);
            if raw_score <= 0.0 {
                continue;
            }
            let score = (raw_score * entry.confidence - penalty).max(0.0);
            if best.as_ref().is_none_or(|b| score > b.score) {
                best = Some(Resolution {
                    key: entry.key.clone(),
                    value: entry.value.clone(),
                    score,
                    source: entry.source,
                    sensitive: entry.sensitive,
                    rationale: format!("{rationale} ({} ~ {})", entry.key, label),
                });
            }
        }

        best.filter(|resolution| {
            if resolution.score < self.threshold {
                return false;
            }
            // Never retype a value the field already holds; that is a wasted action and often
            // an infinite loop.
            field.current_value.as_deref().map(|c| c.trim() != resolution.value.trim()).unwrap_or(true)
        })
    }

    /// Record that a value was typed.
    pub fn mark_used(&mut self, key: &str) {
        if let Some(entry) = self.entries.get_mut(key) {
            entry.uses = entry.uses.saturating_add(1);
        }
    }

    pub fn get(&self, key: &str) -> Option<&ValueEntry> {
        self.entries.get(&normalize(key))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Non-sensitive, non-derived entries — the facts a DONE verification can look for on the
    /// page, and the only ones safe to echo to the host.
    pub fn assertable(&self) -> Vec<&ValueEntry> {
        self.entries.values().filter(|e| !e.sensitive && e.source != ValueSource::Derived).collect()
    }

    /// The task's known facts, for the policy model's own context.
    ///
    /// Jev was deciding "one way" vs "round trip" at 0.54/0.39 on a live Google Flights page
    /// while the runtime already held `trip_type: one-way` and was not telling it. The goal text
    /// alone leaves that kind of requirement buried in prose; as structured pairs it is the
    /// thing most likely to settle a close call — and every escalation it prevents is a host
    /// round trip saved, which is the whole point of the project.
    ///
    /// Sensitive values are excluded outright: a password belongs in the field it is typed into
    /// and nowhere else, least of all in a model request.
    pub fn task_facts(&self) -> serde_json::Map<String, serde_json::Value> {
        self.entries
            .values()
            .filter(|entry| !entry.sensitive)
            .map(|entry| (entry.key.clone(), serde_json::json!(entry.value)))
            .collect()
    }

    /// A redacted listing for the host and for traces.
    pub fn summary(&self) -> Vec<serde_json::Value> {
        self.entries
            .values()
            .map(|e| {
                serde_json::json!({
                    "key": e.key,
                    "value": e.redacted(),
                    "source": e.source,
                    "uses": e.uses,
                    "sensitive": e.sensitive,
                })
            })
            .collect()
    }
}

fn scalar_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}
