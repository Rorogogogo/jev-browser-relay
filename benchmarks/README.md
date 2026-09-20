# Benchmarks

Compares a **host-driven browser agent** (the host model participates in every meaningful browser
action) against **jev-browser-relay** (Jev decides continuously; the host is consulted only for
planning, unresolvable values, real ambiguity, confirmation and verification).

```bash
cargo run -- benchmark --trials 5
cargo run -- benchmark --trials 5 --json-out benchmarks/results.json
cargo run -- benchmark --scenario click-heavy --trials 9
```

No API key and no browser required: both modes drive the same scripted page models in
`../fixtures/`, so the browser work is identical and only the *decider* differs.

## Scenarios

| name | what it exercises |
| --- | --- |
| `click-heavy` | mostly navigation — where Jev's advantage should be largest |
| `form-heavy` | eight fields, six resolvable from context — value-pool reuse |
| `mixed` | clicks, typing and a selection together |
| `reasoning-fallback` | forces one `NEEDS_REASONING` transition |
| `safety-gate` | reaches a consequential action and verifies confirmation gating |

## Reading the numbers honestly

- **Measured:** browser actions, browser protocol (CDP) calls, host round trips, Jev requests,
  success rate. Round-trip counts are *structural* — they follow from the architecture, not from
  a stopwatch, so they do not vary between runs.
- **Modelled:** model latency. A host turn is assumed to cost `--host-turn-ms` (default 3500) and
  a Jev request `--jev-ms` (default 320). These are assumptions, printed above every table, and
  they are the only inputs to the wall-clock column that are not measured.
- **Measured live:** pass `--live` to use the real TypeSafe API for Mode B. This needs
  `TYPESAFE_API_KEY` and costs money, so it is opt-in and never part of `cargo test`.

Trials alternate A/B then B/A so neither mode always runs on a warm process, and every trial is
recorded in the JSON output — nothing is cherry-picked.

## Expected shape of the result

Mode B does **more** browser protocol calls (roughly double), because it re-verifies page
freshness before each action. That is the intended trade: CDP calls cost milliseconds and host
turns cost seconds.
