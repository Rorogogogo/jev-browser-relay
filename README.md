# 🏎️ jev-browser-relay

> A browser runtime for AI coding agents that keeps the expensive model out of the click loop.

![License](https://img.shields.io/badge/license-MIT-blue.svg)
![Rust](https://img.shields.io/badge/rust-1.82%2B-orange.svg)
![Tests](https://img.shields.io/badge/tests-74%20passing-brightgreen.svg)
![API keys required](https://img.shields.io/badge/model%20API%20keys-1-blue.svg)

Your coding agent is good at understanding what you want and judging whether it worked. It is an
expensive way to decide which button to press next. So this runtime splits the job.

```
Claude Code / Codex          jev-browser-relay              Browser Harness
(your existing session)      (Rust control plane)           → Chrome
        │                            │
        │  MCP                       │  observe → decide → validate → act → settle
        │  ──────────────────────▶   │  ↻ many times per call
        │                            │
        │  ◀────────────────────────  only when a decision needs a capable model
```

One `jev_browser_run` call executes **many** browser actions. The host model is consulted for
planning, for a value the runtime cannot resolve, for genuine ambiguity, for anything
irreversible, and for final verification. Nothing else.

---

## ✨ Features

- 🔑 **One API key.** `TYPESAFE_API_KEY` and nothing else — no OpenAI, Anthropic, OpenRouter,
  Mercury or Gemini key, not even for the text typed into form fields.
- 🔁 **Many actions per host turn.** The policy model runs the loop; your agent is woken only when
  a decision genuinely needs it.
- 🧠 **Deterministic field filling.** Task context is matched to page fields by meaning, so
  `origin` fills "Where from?" and `surname` fills "Last name" — no second LLM.
- 🛑 **Consequential actions are gated.** Purchases, orders, sends and deletions stop for
  confirmation before anything executes.
- 🔍 **`DONE` is verified, not trusted.** The runtime re-checks the outcome itself and hands the
  page back when it cannot settle the question.
- 🧷 **Stale-decision protection.** Every action is re-validated against page identity and
  hit-tested immediately before input.
- 🧹 **One task, one session, one tab.** Duplicate starts resume the live session; tabs orphaned
  by a crashed run are reclaimed by the next one.
- 🚫 **No code from the model.** Jev picks an index from a bounded set. Selectors, XPath,
  coordinates and JavaScript are not expressible in the answer format.

---

## 📸 Demo

A real run against the live site — one host turn, start to verified finish:

```console
$ jev-browser-relay run \
    --url "https://www.wikipedia.org" \
    --goal "Search Wikipedia in English for Godel's incompleteness theorems and open that article." \
    --context '{"query":"Godel incompleteness theorems","language":"English"}'

{
  "status": "done",
  "verification": {
    "verdict": "verified",
    "host_verification_required": false,
    "final_title": "Gödel's incompleteness theorems - Wikipedia",
    "final_url": "https://en.wikipedia.org/wiki/G%C3%B6del's_incompleteness_theorems"
  },
  "steps": 2
}

{ "browser_actions": 2, "host_round_trips": 1, "values_resolved_locally": 1, "total_task_ms": 3098 }
```

<!-- Add a terminal GIF of a Google Flights run here -->

---

## 📦 Installation

```bash
cargo install --path crates/jev-browser-relay-cli
export TYPESAFE_API_KEY=...          # the only key you need

jev-browser-relay setup
```

`setup` does the rest in one go: installs
[Browser Harness](https://github.com/browser-use/browser-harness) if missing (asking first — it is
the only command here that installs anything), starts the daemon, attaches to Chrome, closes tabs
left by earlier runs, and registers the MCP server with Claude Code and Codex. It is idempotent.

> **One step cannot be automated.** Chrome asks you once, in its own UI, to allow remote
> debugging. `setup` opens `chrome://inspect/#remote-debugging` and tells you to tick the box.
> That is a consent gesture, and it should stay one.

| Command | Purpose |
| --- | --- |
| `jev-browser-relay setup --yes` | Non-interactive: install without prompting |
| `jev-browser-relay setup --no-register` | Skip touching your agent config |
| `jev-browser-relay doctor` | Check everything, change nothing |

You never need to start the daemon by hand — any session that finds it down starts it.

---

## 🚀 Quick Start

Say to Claude Code:

> Find one-way flights from Sydney to Tokyo on October 10.

It derives the structured context, starts a session, and calls `jev_browser_run` **once**:

```json
{
  "url": "https://www.google.com/travel/flights?hl=en",
  "goal": "Find one-way flights from Sydney to Tokyo on 2026-10-10. Stop when flight options are visible.",
  "context": {
    "origin": "Sydney",
    "destination": "Tokyo",
    "departure_date": "2026-10-10",
    "trip_type": "one-way",
    "cabin": "economy"
  }
}
```

The runtime does the rest:

```
Jev → SELECT one way          Jev → TYPE_TEXT "Where from?"  runtime → "Sydney"  (from context)
Jev → TYPE_TEXT "Where to?"   runtime → "Tokyo"              Jev → TYPE_TEXT date
Jev → CLICK Search            Jev → DONE                     runtime → verify
```

No Chrome and no API key? Everything below runs offline against scripted page models:

```bash
cargo run -- benchmark --trials 5
cargo run -- inspect --fixture flights
cargo run -- mcp --fixture flights    # show an agent the whole flow with nothing installed
```

---

## 📖 Usage

### MCP tools

Seven stateful tools built around a **task session**, not around clicks. There is deliberately no
`click` tool — exposing one would put the host straight back into the per-action loop.

| Tool | Purpose |
| --- | --- |
| `jev_browser_start` | `{url, goal, context}` → `session_id` |
| `jev_browser_run` | Executes many actions; returns on the first thing needing a human-grade decision |
| `jev_browser_provide_input` | Answer a `needs_input` pause |
| `jev_browser_provide_reasoning` | Answer a `needs_reasoning` pause |
| `jev_browser_confirm` | Approve or decline a gated consequential action |
| `jev_browser_observe` | Read the page without acting |
| `jev_browser_stop` | End the session, return the metrics report |

`jev_browser_run` returns one of `done`, `needs_input`, `needs_reasoning`, `needs_confirmation`,
`blocked`, `budget_exceeded` or `error`. Every reply carries a `next` field saying what to do.
Keep the `session_id` — answering a pause resumes the task rather than restarting it.

### CLI

```bash
jev-browser-relay setup                        # install and wire up everything
jev-browser-relay mcp                          # what your agent launches
jev-browser-relay run --url … --goal … --context '{…}'
jev-browser-relay inspect --fixture flights    # what the runtime sees on a page
jev-browser-relay benchmark --trials 5
jev-browser-relay doctor
```

### Configuration

Non-secret tuning only; the key is never read from a config file.

| Variable | Default | Effect |
| --- | --- | --- |
| `TYPESAFE_API_KEY` | — | **Required.** The only model API key. |
| `TYPESAFE_MODEL` | `jev-latest` | Policy model id |
| `TYPESAFE_ENDPOINT` | `api.typesafe.ai/v1/systemone` | Override the API endpoint |
| `JEV_RELAY_MAX_STEPS` | `30` | Browser actions per `run` call |
| `JEV_RELAY_MAX_DURATION_MS` | `60000` | Wall-clock budget per `run` call |
| `JEV_RELAY_VALUE_THRESHOLD` | `0.6` | Below this, ask the host instead of guessing a field value |
| `JEV_RELAY_REASONING_THRESHOLD` | `0.35` | Below this **and** with no front-runner, escalate |
| `JEV_RELAY_CONSEQUENTIAL_PHRASES` | — | Extra comma-separated phrases to gate |
| `JEV_RELAY_DISABLE_SAFETY` | — | Set to `1` to disable the confirmation gate |
| `JEV_RELAY_DISABLE_REASONING` | — | Set to `1` to never escalate for ambiguity |
| `JEV_RELAY_LOG` | `info` | Tracing filter. Always writes to stderr, never stdout |

### The Skill

Optional orchestration guidance that teaches a host to use the runtime well — front-load context,
call `run` once, answer only what it asks for.

```bash
cp -r skills/claude-code/jev-browser-relay ~/.claude/skills/   # Claude Code
cat skills/codex/AGENTS.md >> ~/.codex/AGENTS.md               # Codex
```

The Skill is guidance; the MCP server is the capability.

---

## 📊 Results

### Live, against real sites and the real API

| Task | Host turns | Browser actions | Wall clock | Verdict |
| --- | ---: | ---: | ---: | --- |
| Wikipedia search → open article | **1** | 2 | ~3.1 s | verified |
| Google Flights Sydney→Tokyo one-way | **3 – 11** | 8 – 10 | 6.5 – 8.4 s | verified |

The Wikipedia figure is stable. **The Google Flights figure is not**, and the spread is the honest
headline: across seven runs it ranged from 3 to 11 host turns, driven by how often the policy model
asked for help on near-identical controls. Seven runs is not enough to separate page variation from
model non-determinism, and no claim is made about which it is.

The reasoning threshold that governs escalation is configurable, and has deliberately **not** been
tuned down to flatter these numbers — every escalation it suppresses is a decision the model said
it was unsure about.

### Scripted benchmark

`cargo run -- benchmark --trials 5`, five scenarios, alternating trials:

| Scenario | Host-driven | jev-browser-relay | Actions | Success |
| --- | ---: | ---: | ---: | ---: |
| click-heavy | 7.0 | **1.0** | 4 | 100% |
| form-heavy | 12.0 | **3.0** | 9 | 100% |
| mixed | 8.0 | **1.0** | 5 | 100% |
| reasoning-fallback | 5.0 | **3.0** | 1 | 100% |
| safety-gate | 6.0 | **3.0** | 3 | 100% |

**38 host-model turns → 11, a 71% reduction; 86% on the click-heavy task.**

Read that in light of the live numbers. On scripted scenarios the relay does 5 browser actions per
host turn; on a hard, dynamic site it can drop to roughly 1, close to break-even against a
host-driven agent. The mechanism works — but *how much* it saves depends heavily on the site, and
the benchmark is the optimistic end of the range, not a typical one.

Three caveats, stated up front:

- **Round-trip counts are exact.** They are structural, not timing-dependent.
- **Model latency is modelled, not measured** (`--host-turn-ms`, `--jev-ms`), unless `--live`.
  The benchmark prints this above every table.
- **The relay issues more CDP calls, not fewer** — roughly double, because it re-verifies page
  freshness before every action. CDP calls cost milliseconds; host turns cost seconds.

---

## 🏗️ Architecture

```
crates/
  jev-browser-relay-core/     state machine, value pool, choice space, safety, verification
  jev-browser-relay-jev/      TypeSafe transport (the only external model API)
  jev-browser-relay-browser/  Browser Harness over CDP + a scripted backend
  jev-browser-relay-mcp/      stdio MCP server
  jev-browser-relay-cli/      the `jev-browser-relay` binary
fixtures/                     scenario page models, shared by tests and benchmarks
docs/ARCHITECTURE.md          the reconnaissance note this was built from
```

Both external dependencies are traits — `JevTransport` and `BrowserBackend` — so the entire policy
is exercised with no network and no Chrome.

**Rust owns the control plane and nothing else.** Browser Use and Browser Harness are not
rewritten and no parallel CDP stack exists. The harness daemon speaks newline-delimited JSON over a
Unix socket, which is a language-agnostic boundary, so the runtime drives Chrome from Rust
directly — **no Python in the hot path**. Python is needed only to install and run the daemon.

<details>
<summary><b>How the policy loop works</b></summary>

One TypeSafe request per cycle answers **both** "which operation" and "which element": the runtime
asks them as separate heads of the same request and uses only the head the chosen operation
selects. Two decisions, one network round trip.

Jev picks an index from a choice space built from what the runtime just observed, so selectors,
XPath, coordinates and JavaScript are not representable in the answer. A response is rejected
unless the choice was offered, the probabilities cover exactly the offered ids, they sum to 1, and
the choice is the argmax. Each operation sees only compatible targets.

</details>

<details>
<summary><b>TYPE_TEXT without a second LLM</b></summary>

When Jev selects `TYPE_TEXT`, the runtime resolves the value itself, in order: structured task
context → values the host supplied earlier → session facts (url, title, today) → semantic aliases
over a normalized key space.

Matching is a small explainable scorer, not local NLP. Diacritics fold, so `Godel` matches `Gödel`
and `Zurich` matches `Zürich`. Distinct meanings never cross-resolve: an outbound date will not be
typed into "Return date". Below the confidence threshold the loop pauses rather than guessing, the
host answers once, and the value is cached under its meaning.

</details>

<details>
<summary><b>Safety, verification and de-duplication</b></summary>

**Safety.** Consequential actions are gated before anything executes. Classification is
deterministic and conservative — a false positive costs one round trip, a false negative costs real
money. It is also tuned not to cry wolf: "Continue to checkout" only navigates, so it is not gated;
"Place order" is. An approval binds to one action on one page.

**Verification.** `DONE` is a claim. The runtime re-observes and checks it: are the task's values
visible (allowing for surface variation), did navigation progress, is there an error state? Weak
signals cannot veto strong ones, and a single unrendered value cannot fail an otherwise good run —
richer context must help verification, not sabotage it. When the checks cannot settle it, the
session reports `done` with `host_verification_required` and hands back the page.

**De-duplication.** Starting a url+goal already open returns the live session with `reused: true`
instead of a second tab. Tabs are recorded with the pid that opened them, so a crashed run's tabs
are reclaimed by the next one. An out-of-date Browser Harness is upgraded automatically, but only
while **no session is running** — upgrading restarts the daemon and would otherwise kill a browser
task in flight.

**Secrets.** Keys that look sensitive are flagged on arrival. Their values are never logged, never
placed in a model request, never echoed to the host, and never used as verification evidence.

</details>

---

## 🧪 Tests

```bash
cargo test
```

**74 tests. No API key, no browser, no network.** The Jev double reads the real request body and
answers over the actual offered choice space, so request construction and response validation are
exercised for real. Live API use is opt-in and separate: `benchmark --live`.

Coverage includes session lifecycle, Jev request/response handling, operation compatibility, target
filtering, value resolution and alias lookup, all three pause/resume paths, stale-element
protection, DONE verification, retry budgets, timeouts, browser disconnect, malformed and failing
responses, multiple sessions, metrics, secret redaction, task de-duplication, tab reclamation,
concurrent state writes, and the MCP surface end to end.

---

## 🤝 Contributing

Issues and pull requests welcome. Before opening a PR:

```bash
cargo fmt && cargo clippy --all-targets && cargo test
```

All three must be clean. New behaviour needs a test that fails without it — the bugs worth fixing
here were all found by a test or by a real run, not by reading the code.

---

## 📄 License

[MIT](LICENSE).

Built on [browser-use](https://github.com/browser-use)'s work.
[jev-ultrafast](https://github.com/browser-use/jev-ultrafast) is the direct ancestor of the policy
loop — the dynamic indexed action space, the single-request operation/target decision, the freshness
guards and the adaptive settle are its ideas, and `snapshot.js` is its code.
[browser-harness](https://github.com/browser-use/browser-harness) is the browser layer. Both are MIT.
