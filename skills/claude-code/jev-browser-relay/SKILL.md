---
name: jev-browser-relay
description: Run browser tasks — searching, filling forms, navigating sites, extracting information from pages — through the jev-browser-relay MCP server, which executes many browser actions per call instead of one action per turn. Use whenever a task needs a real browser and the jev_browser_* tools are available. Do not use for reading a single static URL, which WebFetch handles better.
---

# jev-browser-relay

A fast policy model drives the browser. You plan, fill gaps, and verify.

This matters because the default way to automate a browser — look at the page, decide one
action, execute, look again — costs one of your turns per click. A ten-action task becomes ten
turns. This runtime collapses that: **one `jev_browser_run` call executes many browser actions**
and returns only when something genuinely needs you.

## The shape of a task

```
jev_browser_start  →  jev_browser_run  →  [answer 0–2 questions]  →  verify  →  jev_browser_stop
```

Three or four tool calls for a whole task. Not twenty.

## 1. Start: front-load the context

`jev_browser_start` takes `url`, `goal`, and — the part that does the work — a `context` object.

The runtime matches your context keys to page fields **by meaning**, so `origin` fills a field
labelled "Where from?" and `surname` fills "Last name". Every value you put here is a pause that
never happens.

```json
{
  "url": "https://www.google.com/travel/flights?hl=en",
  "goal": "Find one-way flights from Sydney to Tokyo on 10 October 2026 for one adult in economy. Stop when matching flight options are visible.",
  "context": {
    "origin": "Sydney",
    "destination": "Tokyo",
    "date": "2026-10-10",
    "trip_type": "one-way",
    "passengers": "1",
    "cabin": "economy"
  }
}
```

Guidance:

- **Field names are free-form.** Use whatever fits the task. Nested objects are fine.
- **Extract every value the user implied**, including ones you are not sure are needed. An unused
  key costs nothing; a missing one costs a round trip.
- **Write the goal as a complete success condition**, including how to know when to stop. The
  policy model reads it on every step.
- Do not put secrets in `context` unless the task truly needs them. Keys that look sensitive
  (password, card number, OTP) are flagged, redacted from logs, and never sent to the policy model.

## 2. Run: let it work

```json
{ "session_id": "ses_...", "max_steps": 30 }
```

**Call this once and read the result.** Do not call it in a loop expecting one action per call —
that recreates the exact cost this runtime removes.

It returns one of:

| status | what to do |
| --- | --- |
| `done` | Check `verification` (see step 4). |
| `needs_input` | A field's value could not be resolved. Call `jev_browser_provide_input`. |
| `needs_reasoning` | Genuine ambiguity. Call `jev_browser_provide_reasoning`. |
| `needs_confirmation` | A consequential action is gated. See step 3. |
| `budget_exceeded` | Not a failure. Just call `jev_browser_run` again. |
| `blocked` | It cannot proceed. `jev_browser_observe` to see why. |
| `error` | Read `error.code`. Most are recoverable by running again. |

Every reply carries a `next` field telling you what to do. Keep the `session_id`: answering a
pause resumes the same task, it does not restart it.

## 3. Answering pauses

**`needs_input`** — give the exact string to type, nothing else:

```json
{ "session_id": "ses_...", "request_id": "req_...", "value": "Tokyo" }
```

It is cached under that field's meaning, so synonyms of the same field never ask again.

**`needs_reasoning`** — you get the page summary and the candidate actions with their
probabilities. Reply with one or two sentences naming what to do **now**; it steers the next
decision only, so a long plan is wasted.

```json
{ "session_id": "ses_...", "request_id": "req_...", "guidance": "Choose the 07:05 Qantas nonstop; the user asked for the earliest direct flight." }
```

**`needs_confirmation`** — the runtime has stopped before something irreversible: a purchase, an
order, a send, a deletion. **Ask the user unless they already authorised this exact action.**
Then:

```json
{ "session_id": "ses_...", "request_id": "req_...", "approved": true }
```

Approval covers that one action on that one page. It does not carry over.

## 4. Verify before you claim success

`done` includes a `verification` block. The runtime checks the outcome independently — it never
reports success just because the policy model said so.

- `verdict: "verified"` — the task values are visible on the final page. Report success.
- `host_verification_required: true` — **look yourself.** Read `page_excerpt`, or call
  `jev_browser_observe`, and judge. Do not report success on the runtime's word alone.

Then `jev_browser_stop` for the final report.

## Reading results off the page

Use `jev_browser_observe` to read the current page: url, title, visible text, and the indexed
interactive elements. It changes nothing. Pass `screenshot: true` only when the task is genuinely
visual — the normal loop needs no pixels and screenshots are slow.

## Anti-patterns

- ❌ Calling `jev_browser_run` repeatedly, one action at a time. One call does the whole task.
- ❌ Starting with a thin `context` and answering `needs_input` five times. Front-load it.
- ❌ A vague goal like "search for flights". The policy model cannot tell when to stop.
- ❌ Reporting success when `host_verification_required` is true.
- ❌ Approving a `needs_confirmation` on the user's behalf without asking.
- ❌ Starting a new session because a task paused. Resume the one you have. (Starting the same
  url and goal again returns the live session with `reused: true` rather than opening a second
  tab — but rely on `session_id`, not on that safety net.)

## When not to use this

Reading one static page is faster with `WebFetch`. This runtime is for tasks that need a real
browser: interaction, logged-in state, forms, multi-step navigation.
