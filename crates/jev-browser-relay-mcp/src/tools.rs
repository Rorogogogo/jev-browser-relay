//! The MCP tool surface.
//!
//! Seven stateful tools built around a *task session*, not around raw browser commands. There is
//! deliberately no `click` tool and no `type` tool: exposing those would put the host model back
//! in the per-action loop, which is the exact cost this project exists to remove.

use serde_json::{json, Value};

pub const START: &str = "jev_browser_start";
pub const RUN: &str = "jev_browser_run";
pub const PROVIDE_INPUT: &str = "jev_browser_provide_input";
pub const PROVIDE_REASONING: &str = "jev_browser_provide_reasoning";
pub const CONFIRM: &str = "jev_browser_confirm";
pub const OBSERVE: &str = "jev_browser_observe";
pub const STOP: &str = "jev_browser_stop";

pub fn definitions() -> Vec<Value> {
    vec![
        json!({
            "name": START,
            "description": "Open a browser session for one task. Supply the goal in plain language and, \
                            crucially, a `context` object holding every value the task already implies \
                            (origin, destination, dates, names, search terms). The runtime resolves \
                            form fields from that object by meaning, so each value you provide here is \
                            a round trip you will not be asked to make later. Returns a session_id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "The page to start on." },
                    "goal": {
                        "type": "string",
                        "description": "What a successful outcome looks like, stated completely. The \
                                        policy model sees this on every step."
                    },
                    "context": {
                        "type": "object",
                        "description": "Task values as free-form key/value pairs. Field names are NOT \
                                        fixed: use whatever names fit the task. Nested objects are \
                                        flattened. Keys are matched to page fields semantically, so \
                                        \"origin\" fills \"Where from?\".",
                        "additionalProperties": true
                    },
                    "force_new": {
                        "type": "boolean",
                        "description": "Open a second independent session even if this exact task is \
                                        already open. Off by default: starting the same url and goal \
                                        twice returns the live session with reused:true, so a paused \
                                        task resumes instead of being abandoned behind a stray tab.",
                        "default": false
                    }
                },
                "required": ["url", "goal"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": RUN,
            "description": "Run the task autonomously. ONE call executes MANY browser actions — clicking, \
                            typing, selecting, scrolling — without involving you. It returns only when \
                            something genuinely needs you: done, needs_input, needs_reasoning, \
                            needs_confirmation, blocked, budget_exceeded, or error. Do not call this in a \
                            loop expecting one action per call; call it, read the status, and respond to \
                            what it asks for. On budget_exceeded, simply call it again.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "max_steps": {
                        "type": "integer",
                        "description": "Browser actions this call may execute. Default 30.",
                        "minimum": 1
                    },
                    "max_duration_ms": {
                        "type": "integer",
                        "description": "Wall-clock budget for this call. Default 60000.",
                        "minimum": 100
                    }
                },
                "required": ["session_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": PROVIDE_INPUT,
            "description": "Answer a needs_input pause with the text for one field, then the run continues \
                            from where it stopped — the task is not restarted. The value is cached under \
                            that field's meaning, so synonyms of the same field resolve without asking again.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "request_id": { "type": "string", "description": "From the needs_input reply." },
                    "value": { "type": "string", "description": "Exactly what should be typed." },
                    "remember": {
                        "type": "boolean",
                        "description": "Reuse this value for semantically equivalent fields. Default true.",
                        "default": true
                    }
                },
                "required": ["session_id", "request_id", "value"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": PROVIDE_REASONING,
            "description": "Answer a needs_reasoning pause with short, concrete guidance — which candidate \
                            to take and why. It steers the next decision only, so say what to do now rather \
                            than describing a whole plan.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "request_id": { "type": "string" },
                    "guidance": {
                        "type": "string",
                        "description": "One or two sentences naming the action to take and the reason."
                    }
                },
                "required": ["session_id", "request_id", "guidance"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": CONFIRM,
            "description": "Approve or decline a consequential action the runtime gated — a purchase, an \
                            order, an irreversible send, a deletion. Approval covers that one action on that \
                            one page and does not carry to the next. Declining blocks the session.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "request_id": { "type": "string" },
                    "approved": { "type": "boolean" }
                },
                "required": ["session_id", "request_id", "approved"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": OBSERVE,
            "description": "Read the current page without acting: url, title, visible text, the indexed \
                            interactive elements, recent actions, and what the value pool holds. Use it to \
                            verify a result or to understand a pause. It changes nothing.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "screenshot": {
                        "type": "boolean",
                        "description": "Include a JPEG screenshot. Off by default; the normal loop needs no \
                                        pixels and screenshots are slow.",
                        "default": false
                    }
                },
                "required": ["session_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": STOP,
            "description": "End the session, close its browser tab, and return the performance report — \
                            browser actions, host round trips, Jev requests and latencies, stale decisions, \
                            and values resolved without asking you.",
            "inputSchema": {
                "type": "object",
                "properties": { "session_id": { "type": "string" } },
                "required": ["session_id"],
                "additionalProperties": false
            }
        }),
    ]
}
