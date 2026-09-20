# jev-browser-relay

**A browser runtime for AI coding agents that keeps the expensive model out of the click loop.**

Your coding agent is good at understanding what you want and judging whether it worked. It is an
expensive way to decide which button to press next. So this runtime splits the job:

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

**`TYPESAFE_API_KEY` is the only model API key required.** No OpenAI, Anthropic, OpenRouter,
Mercury or Gemini key — not even for generating the text typed into form fields.

---

## The result

From `jev-browser-relay benchmark`, five scenarios, five alternating trials each:

| scenario | mode | host turns | browser actions | success |
| --- | --- | ---: | ---: | ---: |
| click-heavy | host-driven | 7.0 | 4 | 100% |
| click-heavy | **jev-browser-relay** | **1.0** | 4 | 100% |
| form-heavy | host-driven | 12.0 | 9 | 100% |
| form-heavy | **jev-browser-relay** | **3.0** | 9 | 100% |
| mixed | host-driven | 8.0 | 5 | 100% |
| mixed | **jev-browser-relay** | **1.0** | 5 | 100% |
| reasoning-fallback | host-driven | 5.0 | 1 | 100% |
| reasoning-fallback | **jev-browser-relay** | **3.0** | 1 | 100% |
| safety-gate | host-driven | 6.0 | 3 | 100% |
| safety-gate | **jev-browser-relay** | **3.0** | 3 | 100% |

**38 host-model turns → 11. A 71% reduction; 86% on the click-heavy task.**

Two honest notes on reading this:

- **Round-trip counts are exact.** They are structural, not timing-dependent: both modes drive
  the same page model and take the same decisions, so the only difference is who is asked.
- **Model latency is modelled, not measured** (`--host-turn-ms`, default 3500; `--jev-ms`,
  default 320), unless you pass `--live`. The benchmark prints this above every table so a
  projection is never mistaken for a measurement.
- **The relay issues *more* CDP calls, not fewer** — roughly double, because it re-verifies page
  freshness before every action. That is the intended trade: local browser calls cost
  milliseconds, host-model turns cost seconds.

Reproduce it with no API key and no browser:

```bash
cargo run -- benchmark --trials 5
```

---

## Install

```bash
cargo install --path crates/jev-browser-relay-cli
export TYPESAFE_API_KEY=...          # the only key you need
```

The browser layer is [Browser Harness](https://github.com/browser-use/browser-harness), which
owns the CDP connection to your real Chrome:

```bash
uv tool install browser-harness
browser-harness --doctor             # then tick the box on chrome://inspect/#remote-debugging
```

Check everything:

```bash
jev-browser-relay doctor
```

Connect it to your agent:

```bash
# Claude Code
claude mcp add jev-browser-relay -- jev-browser-relay mcp
```

```toml
# Codex — ~/.codex/config.toml
[mcp_servers.jev-browser-relay]
command = "jev-browser-relay"
args = ["mcp"]
```

Optionally install the skill that teaches the host to use it well — copy
`skills/claude-code/jev-browser-relay/` into `~/.claude/skills/`, or append
`skills/codex/AGENTS.md` to your Codex instructions. The skill is guidance; the MCP server is the
capability.

---

## Using it

Say to Claude Code:

> Find one-way flights from Sydney to Tokyo on October 10.

It derives the structured context and starts a session:

```json
{ "url": "https://www.google.com/travel/flights?hl=en",
  "goal": "Find one-way flights from Sydney to Tokyo on 2026-10-10. Stop when flight options are visible.",
  "context": { "origin": "Sydney", "destination": "Tokyo", "date": "2026-10-10", "trip_type": "one-way" } }
```

Then calls `jev_browser_run` **once**, and the runtime does the rest:

```
Jev → SELECT one way        Jev → TYPE_TEXT "Where from?"   runtime → "Sydney"   (from context)
Jev → TYPE_TEXT "Where to?" runtime → "Tokyo"               Jev → TYPE_TEXT date
Jev → CLICK Search          Jev → DONE                      runtime → verify
```

`done`, with a verification report. One host turn for the whole task.

### The MCP surface

Seven stateful tools built around a **task session**, not around clicks. There is deliberately no
`click` tool — exposing one would put the host straight back into the per-action loop.

| tool | purpose |
| --- | --- |
| `jev_browser_start` | `{url, goal, context}` → `session_id` |
| `jev_browser_run` | executes many actions; returns on the first thing needing a human-grade decision |
| `jev_browser_provide_input` | answer a `needs_input` pause |
| `jev_browser_provide_reasoning` | answer a `needs_reasoning` pause |
| `jev_browser_confirm` | approve or decline a gated consequential action |
| `jev_browser_observe` | read the page without acting |
| `jev_browser_stop` | end the session, return the metrics report |

### CLI

```bash
jev-browser-relay mcp                          # what your agent launches
jev-browser-relay run --url … --goal … --context '{…}'
jev-browser-relay inspect --fixture flights    # what the runtime sees on a page
jev-browser-relay benchmark --trials 5
jev-browser-relay doctor
```

`--fixture` runs against a scripted page model: no Chrome, no daemon, no API key for the browser
side. `mcp --fixture flights` even lets you show an agent the whole flow with nothing installed.

---

## How it works

### The policy loop

```
observe → decide → validate freshness → act → settle → observe → …
```

One TypeSafe request per cycle answers **both** "which operation" and "which element", because
the runtime asks them as separate heads of the same request and uses only the head the chosen
operation selects. Two decisions, one network round trip — the
[jev-ultrafast](https://github.com/browser-use/jev-ultrafast) design, which this project builds on.

### The model cannot express a dangerous action

Jev picks an index from a choice space the runtime constructed from what it just observed. It
never emits selectors, XPath, coordinates, JavaScript or shell commands — those are not
representable in the answer format. A response is rejected unless the choice was offered, the
probabilities cover exactly the offered ids, they sum to 1, and the choice is the argmax. An
invalid answer never becomes an action.

Each operation gets only compatible targets: `CLICK` sees clickables, `TYPE_TEXT` sees editable
fields, `SELECT` sees actual `<option>` values.

### Stale-decision protection

Every snapshot carries an opaque page marker and a per-node guard. Before an action executes, the
runtime re-checks that the element it chose is still the element it saw; the executor then
re-resolves geometry and hit-tests the click point, rejecting anything moved, hidden or covered.
A stale decision is discarded and retaken, never executed against a changed page, and the count
is reported.

### TYPE_TEXT without a second LLM

This is the part that removes the extra API key. When Jev selects `TYPE_TEXT`, the runtime
resolves the value itself, in order:

1. the structured task context
2. values the host supplied earlier in this session
3. deterministic session facts (current url, title, today)
4. semantic aliases over a normalized key space

So `origin` fills "Where from?", `surname` fills "Last name", and `traveller.last_name` still
reaches "Surname". Matching is a small, explainable scorer — not local NLP — and distinct
meanings never cross-resolve: an outbound date will not be typed into "Return date".

When nothing clears the confidence threshold, the loop **pauses** rather than guessing. The host
answers once, and the value is cached under its meaning, so every later field with that meaning
resolves for free.

### Safety

Consequential actions — purchases, orders, payments, irreversible sends, deletions, publishing,
account changes — are gated behind `needs_confirmation` before anything executes. Classification
is deterministic and deliberately conservative: a false positive costs one host round trip, a
false negative costs real money.

It is also tuned not to cry wolf. "Continue to checkout" only *navigates*, so it is not gated;
"Place order" is. Ambiguous verbs like "Submit" are gated only where the page shows real
finalization signals. An approval binds to one action on one page and does not carry to the next.

### DONE is verified, not trusted

Jev choosing `DONE` is a claim. The runtime re-observes and checks it deterministically: are the
task's own values visible on the final page (allowing for surface variation — `2026-10-10`
rendered as "10 Oct 2026"), did navigation progress, is there an error state? When the checks
cannot settle it, the session still reports `done` but sets `host_verification_required` and
hands back the page, so the host looks before anyone claims success.

### Secrets

Keys that look sensitive are flagged on arrival. Their values are never logged, never placed in a
Jev request, never echoed to the host, and never used as verification evidence — they are only
ever typed into the field they belong to. There is a test that asserts it.

---

## Architecture

```
crates/
  jev-browser-relay-core/     state machine, value pool, choice space, safety, verification, metrics
  jev-browser-relay-jev/      TypeSafe transport (the only external model API)
  jev-browser-relay-browser/  Browser Harness over CDP + a scripted backend for tests
  jev-browser-relay-mcp/      stdio MCP server
  jev-browser-relay-cli/      the `jev-browser-relay` binary
fixtures/                     scenario page models, shared by tests and benchmarks
tests/                        integration tests — no API key, no browser
docs/ARCHITECTURE.md          the reconnaissance note this was built from
```

Both external dependencies are traits — `JevTransport` and `BrowserBackend` — so the entire
policy is exercised with no network and no Chrome.

**Rust owns the control plane and nothing else.** Browser Use and Browser Harness are not
rewritten and no parallel CDP stack exists. The one thing worth knowing: the harness daemon
speaks newline-delimited JSON over a Unix socket, which is a language-agnostic boundary, so the
runtime drives Chrome from Rust directly — **no Python in the hot path**. Python is needed only
to install and run the daemon, out of band.

`snapshot.js` is vendored from jev-ultrafast (MIT) and stays JavaScript because it runs *inside
the page*; porting it to Rust would still mean shipping a script to the document.

---

## Tests

```bash
cargo test
```

52 tests. **No API key, no browser, no network.** The Jev double reads the real request body and
answers over the actual offered choice space, so request construction and response validation are
exercised for real. Live TypeSafe usage is opt-in and separate: `benchmark --live`.

Coverage includes session lifecycle, request construction, response parsing, operation
compatibility, target filtering, value resolution, alias lookup, all three pause/resume paths,
stale-element protection, DONE verification, retry budgets, timeouts, browser disconnect,
malformed and failing Jev responses, multiple sessions, metrics, secret redaction, and the MCP
surface end to end.

---

## Credits and licence

Built on [browser-use](https://github.com/browser-use)'s work.
[jev-ultrafast](https://github.com/browser-use/jev-ultrafast) is the direct ancestor of the policy
loop — the dynamic indexed action space, the single-request operation/target decision, the
freshness guards and the adaptive settle are all its ideas, and `snapshot.js` is its code.
[browser-harness](https://github.com/browser-use/browser-harness) is the browser layer.

MIT, as are all three upstream projects.
