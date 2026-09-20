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

jev-browser-relay setup
```

`setup` does the rest in one go: installs
[Browser Harness](https://github.com/browser-use/browser-harness) if it is missing (asking
first — it is the only command here that installs anything), starts the daemon, attaches to
Chrome, closes any tabs left behind by earlier runs, and registers the MCP server with Claude
Code and Codex. It is idempotent, so running it again just reports the state.

There is exactly **one step nothing can automate**: Chrome asks you once, in its own UI, to allow
remote debugging. `setup` opens `chrome://inspect/#remote-debugging` and tells you to tick the
box. That is a consent gesture, and it should stay one.

```bash
jev-browser-relay setup --yes          # non-interactive: install without prompting
jev-browser-relay setup --no-register  # skip touching your agent config
jev-browser-relay doctor               # check without changing anything
```

You never need to start the daemon by hand. Any session that finds it down starts it, so
`setup` is a convenience and a diagnostic, not a prerequisite.

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
jev-browser-relay setup                        # install and wire up everything
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

### One task, one session, one tab

Two things duplicate in a runtime like this, and both are handled the same way: reuse what
exists, and only ever create when nothing is there.

**Sessions.** `jev_browser_start` with a url and goal already open returns the *live* session,
with `reused: true`, instead of opening a second tab. This is aimed at a specific failure: an
agent hits `needs_input`, gets confused, and starts the task over — stranding the paused session
and its browser tab. Reuse turns that into a resume. A finished task is never reused (starting
again should start again), genuinely different tasks are never merged, and `force_new: true`
opts out.

**The browser layer.** One `ensure` path decides between reusing a running daemon, starting one,
and upgrading it — guarded by a process-wide lock, because two sessions starting together
otherwise both see "nothing running" and both spawn.

**Stray tabs.** Every tab is recorded with the pid that opened it. A runtime that is killed
cannot close its own tabs, so the next run closes them: owner gone means the tab is a leak. Tabs
belonging to a live process are never touched. That record is written under a file lock with
per-process temp files — two runtimes starting at once genuinely do race here, and an earlier
version of it corrupted the file.

### Auto-update, gated on idle

An out-of-date Browser Harness is upgraded automatically, but **only while no session is
running**, and at most once a day.

The reasoning is borrowed from NoMoreIDE's daemon lifecycle: upgrading restarts the daemon, which
drops its CDP connection and every tab being driven through it. Upgrading on sight would mean an
agent that happened to start a session silently killing a browser task in flight — possibly
someone else's, since the daemon is machine-global and shared with other Browser Use tools. Idle
is the one moment where that objection disappears: nothing is lost, and the upgrade costs a
second nobody notices.

A failed or slow upgrade is never fatal. The version already installed still works, and refusing
to run because an optional upgrade did not happen is worse than being one version behind.

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

64 tests. **No API key, no browser, no network.** The Jev double reads the real request body and
answers over the actual offered choice space, so request construction and response validation are
exercised for real. Live TypeSafe usage is opt-in and separate: `benchmark --live`.

Coverage includes session lifecycle, request construction, response parsing, operation
compatibility, target filtering, value resolution, alias lookup, all three pause/resume paths,
stale-element protection, DONE verification, retry budgets, timeouts, browser disconnect,
malformed and failing Jev responses, multiple sessions, metrics, secret redaction, task
de-duplication, tab-ownership reclamation, concurrent state writes, and the MCP surface end to
end.

### What has been exercised against the real thing

The browser layer has run against real Chrome: `jev-browser-relay setup` installs and starts the
Browser Harness daemon, the Rust IPC client drives it over its Unix socket, and
`jev-browser-relay inspect --url …` opens a background tab, injects the snapshot script, and
returns the element table — with the tab closed and its ownership record cleared afterwards. No
API key is needed for any of that, so you can verify it yourself in one command.

**Not yet exercised: decision quality against the live TypeSafe API.** The transport is confirmed
— a request to `api.typesafe.ai` returns a well-formed `401` without a key, so the URL, auth
scheme and body shape are accepted — but how well Jev actually chooses on real pages is not
something the scripted tests can tell you. Set `TYPESAFE_API_KEY` and run
`jev-browser-relay run --url … --goal …` to find out.

---

## Credits and licence

Built on [browser-use](https://github.com/browser-use)'s work.
[jev-ultrafast](https://github.com/browser-use/jev-ultrafast) is the direct ancestor of the policy
loop — the dynamic indexed action space, the single-request operation/target decision, the
freshness guards and the adaptive settle are all its ideas, and `snapshot.js` is its code.
[browser-harness](https://github.com/browser-use/browser-harness) is the browser layer.

MIT, as are all three upstream projects.
