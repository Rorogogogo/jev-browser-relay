# jev-browser-relay — architecture note

Written after reading `browser-use/jev-ultrafast`, `browser-use/browser-harness`, and
`browser-use/browser-use` (all MIT). This is the plan the implementation follows.

## 1. Current relevant upstream architecture

### jev-ultrafast (the closest prior art — ~850 lines, read in full)

The loop is `observe → choose → validate → act → settle`, and the important parts are:

- **One TypeSafe request per decision cycle.** `model.py:choose()` posts to
  `https://api.typesafe.ai/v1/systemone` with a body of
  `{model, state:{page, elements, recent_actions}, questions:{...}}`. Questions are a map of
  independent *choice heads*: `operation`, plus one `<operation>_target` head per operation that
  has compatible targets. Jev answers every head in one round trip; the runtime then uses only
  the head the chosen operation selects. **Two decisions, one network round trip.**
- **Dynamically constructed choice space.** `model.py:action_space()` folds the raw snapshot
  actions into one indexed element table, and builds per-operation target maps so `CLICK` only
  ever offers clickable nodes and `TYPE_TEXT` only editable ones. `SELECT` targets are
  `"<element>:<option>"` pairs carrying an observed option value.
- **The model never emits executable strings.** It picks an index from a bounded set. The index
  maps back to a code-owned DOM node id. No selectors, no XPath, no coordinates, no JS.
- **Response validation.** `validate_choice()` rejects an answer unless the choice is in the
  offered set, the probability keys exactly equal the offered set, every number is finite in
  `[0,1]`, probabilities sum to 1 (±0.02), and the argmax agrees with the choice.
- **Freshness.** `snapshot.js` returns a `marker` (page identity + semantics) and a per-node
  `guards` map. `Browser.fresh()` compares them before acting; `StalePage` forces re-observe and
  re-decide. Geometry is resolved and hit-tested *again* immediately before input, so a covered
  or moved control is rejected rather than mis-clicked.
- **Adaptive settling, not sleeps.** After input it waits for ≥2 animation frames, capped at
  50 ms — or, for a combobox, until visible `[role=option]` suggestions appear, capped at 200 ms.
- **Text generation is a separate, smaller call.** `field_text()` calls an OpenAI-compatible
  endpoint with `TEXT_MODEL_API_KEY` and demands a `{"text": "..."}` JSON object back.

**This last point is the one thing we deliberately change.** Requiring a second LLM key is exactly
what the brief forbids.

### browser-harness (the browser execution layer)

`helpers.cdp()` does not own a browser. It sends one newline-terminated JSON request over a
**Unix domain socket** to a long-lived daemon that holds the CDP websocket to real Chrome:

```
~/.config/browser-harness/runtime/bu-<BU_NAME>.sock      (mode 0600, AF_UNIX)
→ {"method":"Page.navigate","params":{...},"session_id":"..."}\n
← {"result":{...}}   or   {"error":"..."}
```

Plus meta requests: `{"meta":"ping"}` → `{"pong":true,"pid":N,"browser_kind":...}`,
`{"meta":"session"}`, `{"meta":"connection_status"}`, `{"meta":"drain_events"}`.
`Target.*` methods are sent without a session id; everything else defaults to the daemon session.
On Windows the same protocol runs over TCP loopback with a `token` field.

**This is a language-agnostic boundary.** It is the single most important finding in this note:
Rust can drive Chrome through the harness daemon directly, with no Python in the hot path.

### browser-use (the big agent framework)

Large, LLM-driven, screenshot-and-DOM-tree oriented, with its own tool registry and message
manager. It is the wrong shape for a low-latency policy loop and it assumes an LLM key. Its one
concept worth adopting is `sensitive_data` — values that are substituted into fields but never
allowed into model context or logs. We take that idea, not the code.

## 2. Reusable components

| Reused | How |
| --- | --- |
| `snapshot.js` | Vendored near-verbatim (MIT, attributed). It is a genuinely good atomic snapshot: visible-only controls, accessible names, `marker` + per-node `guards`, viewport-clipped text, 250-action cap. Rewriting it in Rust would buy nothing — it must run *in the page*. |
| Harness daemon IPC | Spoken natively from Rust over `tokio::net::UnixStream`. |
| The `systemone` request/response contract | Reimplemented in Rust with the same multi-head shape and stricter validation. |
| Action-space construction | Ported to Rust (`core::action_space`) because the runtime must own the bounded choice space. |
| Execution + occlusion JS | Ported as the `execute`/`settle` page scripts, driven over CDP from Rust. |
| Instruction text | Adapted; `NEXT_ACTION` / `TARGET` rules are good and hard-won. |
| `sensitive_data` concept | Becomes the value pool's `sensitive` flag + log redaction. |

## 3. What stays outside Rust

- Chrome itself and the CDP websocket.
- The browser-harness daemon (Python) — installed once, runs out of band, shared with other tools.
- `snapshot.js` and the execute/settle scripts — they are page-resident by nature.
- Browser compatibility, stealth, proxies, profile management.

We do **not** reimplement Browser Use, Browser Harness, or a CDP stack.

## 4. What belongs in Rust

The whole control plane:

- MCP server (stdio JSON-RPC 2.0).
- Session registry + typed state machine, pause/resume.
- Jev client: request construction, transport, strict response validation.
- The continuous loop: observe → decide → freshness-validate → act → settle.
- Value pool and deterministic semantic resolution (**replaces the second LLM**).
- Host intervention protocol: `NEEDS_INPUT` / `NEEDS_REASONING` / `NEEDS_CONFIRMATION`.
- Safety classifier gating consequential actions.
- Independent DONE verification.
- Retry budgets, timeouts, bounded history.
- Metrics + tracing + redaction.

## 5. Workspace layout

```
jev-browser-relay/
├── crates/
│   ├── jev-browser-relay-core/     types, state machine, value pool, loop, safety,
│   │                               verification, metrics, Jev request build + validate
│   ├── jev-browser-relay-jev/      HTTPS transport to api.typesafe.ai (reqwest)
│   ├── jev-browser-relay-browser/  harness Unix-socket CDP backend + scripted fake backend
│   ├── jev-browser-relay-mcp/      stdio MCP server over the session registry
│   └── jev-browser-relay-cli/      the `jev-browser-relay` binary
├── skills/{claude-code,codex}/
├── tests/          integration tests (mock Jev + scripted browser)
├── benchmarks/     Mode A vs Mode B harness + scenarios
└── docs/
```

Dependency direction is strictly one-way: `cli → mcp → {core, jev, browser}`, and
`jev → core`, `browser → core`. Core depends on neither `reqwest` nor CDP, so the entire
policy is testable with no network and no Chrome.

Transports are traits, so the default test configuration is a mock Jev transport plus a
scripted browser backend. **Normal `cargo test` needs no API key and no Chrome.**

## 6. MCP tool surface

Seven stateful tools, session-centric rather than click-centric:

| Tool | Purpose |
| --- | --- |
| `jev_browser_start` | `{url, goal, context?}` → `{session_id, status}` |
| `jev_browser_run` | `{session_id, max_steps?, max_duration_ms?}` → runs **many** browser actions, returns on the first thing that needs a human-grade decision |
| `jev_browser_provide_input` | `{session_id, request_id, value, remember?}` → resume |
| `jev_browser_provide_reasoning` | `{session_id, request_id, guidance, preferred_action?}` → resume |
| `jev_browser_confirm` | `{session_id, request_id, approved, ...}` → resume or abort |
| `jev_browser_observe` | read-only current state (+ optional screenshot) |
| `jev_browser_stop` | end session, return the metrics report |

`run` is the only tool that costs browser time, and one call is expected to cover the whole task.

## 7. The TYPE_TEXT change (the core contribution)

Upstream calls a second LLM for every text field. We resolve locally, in order:

1. **structured task context** supplied at `start`
2. **previously supplied values** (host input, cached)
3. **deterministic session facts** (current URL, title, today's date)
4. **semantic aliases** over a normalized key space
   (`origin ≡ from ≡ departure_city ≡ "Where from?" ≡ "Leaving from"`)

Scoring is deterministic and explainable. Above the confidence threshold the runtime types the
value and keeps going. Below it, the loop **pauses** and returns `NEEDS_INPUT` with the field
description — the host answers once, the value is cached under its semantic key, and every later
field with the same meaning resolves for free.

This is what removes `TEXT_MODEL_API_KEY`. `TYPESAFE_API_KEY` is the only required key.

## 8. Smallest MVP / first implementation slice

```
Claude Code → MCP stdio → session → Jev decision → browser action → … → result
```

Slice order:

1. core types + action space + value pool + Jev request build/validate (pure, unit-tested)
2. scripted browser backend → a full multi-step autonomous run with **zero** network and no Chrome
3. real Jev transport + real harness backend behind the same traits
4. MCP stdio server + CLI (`mcp`, `run`, `inspect`, `benchmark`, `doctor`)
5. skills, benchmarks, hardening

Step 2 is what makes the vertical slice demonstrable and CI-safe; steps 3's adapters are wired
through the identical traits, so the loop code is the same in both configurations.
