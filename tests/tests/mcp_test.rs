//! The MCP surface as a host agent actually sees it: JSON-RPC frames in, frames out.

use jev_browser_relay_core::config::RuntimeConfig;
use jev_browser_relay_core::snapshot::Operation;
use jev_browser_relay_core::testing::{Planned, ScriptedJev};
use jev_browser_relay_mcp::{RelayServer, ScriptedFactory};
use jev_browser_relay_tests::*;
use serde_json::{json, Value};
use std::sync::Arc;

fn server(fixture: &str, plan: Vec<Planned>) -> RelayServer {
    RelayServer::new(
        Arc::new(ScriptedJev::new(plan)),
        Arc::new(ScriptedFactory { fixture: fixture.to_string() }),
        RuntimeConfig { today: "2026-09-20".into(), ..test_config() },
    )
}

/// Send one frame and return the parsed `result`.
async fn call(server: &RelayServer, id: u32, method: &str, params: Value) -> Value {
    let frame = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string();
    let response = server.handle_line(&frame).await.expect("a request always gets a response");
    let encoded = serde_json::to_value(&response).unwrap();
    assert_eq!(encoded["jsonrpc"], "2.0");
    assert_eq!(encoded["id"], id);
    assert!(encoded.get("error").is_none(), "unexpected protocol error: {encoded}");
    encoded["result"].clone()
}

/// Call a tool and return its structured content.
async fn tool(server: &RelayServer, id: u32, name: &str, arguments: Value) -> Value {
    let result = call(server, id, "tools/call", json!({ "name": name, "arguments": arguments })).await;
    assert!(result["content"].is_array(), "tool results must carry text content");
    result["structuredContent"].clone()
}

#[tokio::test]
async fn the_server_handshakes_and_advertises_its_tools() {
    let server = server(DOCS, vec![]);

    let result = call(&server, 1, "initialize", json!({ "protocolVersion": "2024-11-05" })).await;
    assert_eq!(result["protocolVersion"], "2024-11-05", "the client's version should be honoured");
    assert_eq!(result["serverInfo"]["name"], "jev-browser-relay");
    assert!(result["capabilities"]["tools"].is_object());
    // The instructions are what teach a host not to sit in the click loop.
    assert!(result["instructions"].as_str().unwrap().contains("one call executes many actions"));

    // A notification gets no response at all.
    let notification = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string();
    assert!(server.handle_line(&notification).await.is_none());

    let result = call(&server, 2, "tools/list", json!({})).await;
    let names: Vec<&str> =
        result["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        vec![
            "jev_browser_start",
            "jev_browser_run",
            "jev_browser_provide_input",
            "jev_browser_provide_reasoning",
            "jev_browser_confirm",
            "jev_browser_observe",
            "jev_browser_stop"
        ]
    );
    // No raw browser primitives: exposing them would put the host back in the per-click loop.
    assert!(!names.iter().any(|n| n.contains("click") || n.contains("type") || n.contains("scroll")));
}

#[tokio::test]
async fn a_whole_task_runs_over_mcp_with_one_run_call() {
    let server = server(
        FLIGHTS,
        vec![
            Planned::targeting(Operation::Select, "One way"),
            Planned::targeting(Operation::TypeText, "Where from?"),
            Planned::targeting(Operation::TypeText, "Where to?"),
            Planned::targeting(Operation::TypeText, "Departure"),
            Planned::targeting(Operation::Click, "Search"),
            Planned::new(Operation::Done),
        ],
    );

    let started = tool(
        &server,
        1,
        "jev_browser_start",
        json!({
            "url": "https://fixture.test/flights",
            "goal": "Find one-way flights from Sydney to Tokyo on 10 October 2026",
            "context": { "origin": "Sydney", "destination": "Tokyo", "date": "2026-10-10", "trip_type": "one-way" }
        }),
    )
    .await;
    let session_id = started["session_id"].as_str().expect("a session id").to_string();
    assert_eq!(started["status"], "ready");

    let run = tool(&server, 2, "jev_browser_run", json!({ "session_id": session_id })).await;

    assert_eq!(run["status"], "done", "{run}");
    assert_eq!(run["steps"], 5, "one run call should have executed five browser actions");
    assert_eq!(run["metrics"]["host_round_trips"], 1);
    assert_eq!(run["metrics"]["values_resolved_locally"], 3);
    assert_eq!(run["verification"]["verdict"], "verified");
    assert_eq!(run["verification"]["host_verification_required"], false);

    let stopped = tool(&server, 3, "jev_browser_stop", json!({ "session_id": session_id })).await;
    assert_eq!(stopped["metrics"]["browser_actions"], 5);

    // The session is gone; asking again is a clean, structured error.
    let gone = tool(&server, 4, "jev_browser_run", json!({ "session_id": session_id })).await;
    assert_eq!(gone["error"]["code"], "unknown_session");
}

#[tokio::test]
async fn a_pause_and_resume_round_trip_works_over_mcp() {
    let server = server(
        FLIGHTS,
        vec![
            Planned::targeting(Operation::TypeText, "Where from?"),
            Planned::targeting(Operation::TypeText, "Where to?"),
            Planned::targeting(Operation::TypeText, "Where to?"),
            Planned::targeting(Operation::TypeText, "Departure"),
            Planned::targeting(Operation::Click, "Search"),
            Planned::new(Operation::Done),
        ],
    );

    let started = tool(
        &server,
        1,
        "jev_browser_start",
        json!({
            "url": "https://fixture.test/flights",
            "goal": "Find one-way flights from Sydney on 10 October 2026",
            "context": { "origin": "Sydney", "date": "2026-10-10" }
        }),
    )
    .await;
    let session_id = started["session_id"].as_str().unwrap().to_string();

    let paused = tool(&server, 2, "jev_browser_run", json!({ "session_id": session_id })).await;
    assert_eq!(paused["status"], "needs_input");
    assert_eq!(paused["field"]["label"], "Where to?");
    let request_id = paused["request_id"].as_str().unwrap().to_string();
    assert!(paused["next"].as_str().unwrap().contains("provide_input"));

    let resumed = tool(
        &server,
        3,
        "jev_browser_provide_input",
        json!({ "session_id": session_id, "request_id": request_id, "value": "Tokyo" }),
    )
    .await;
    assert_eq!(resumed["status"], "ready");

    let done = tool(&server, 4, "jev_browser_run", json!({ "session_id": session_id })).await;
    assert_eq!(done["status"], "done", "{done}");
    assert_eq!(done["metrics"]["host_input_requests"], 1);
}

#[tokio::test]
async fn observe_reads_the_page_without_changing_it() {
    let server = server(DOCS, vec![]);
    let started = tool(
        &server,
        1,
        "jev_browser_start",
        json!({ "url": "https://fixture.test/docs", "goal": "Look around" }),
    )
    .await;
    let session_id = started["session_id"].as_str().unwrap().to_string();

    let observed = tool(&server, 2, "jev_browser_observe", json!({ "session_id": session_id })).await;

    assert_eq!(observed["url"], "https://fixture.test/docs");
    assert_eq!(observed["elements"].as_array().unwrap().len(), 3);
    // Screenshots are opt-in: the normal loop never needs pixels.
    assert!(observed["screenshot"].is_null());

    let after = tool(&server, 3, "jev_browser_observe", json!({ "session_id": session_id })).await;
    assert_eq!(observed["url"], after["url"], "observing must not move the page");
}

#[tokio::test]
async fn bad_requests_are_reported_without_breaking_the_stream() {
    let server = server(DOCS, vec![]);

    // Malformed JSON.
    let response = server.handle_line("{not json").await.unwrap();
    let encoded = serde_json::to_value(&response).unwrap();
    assert_eq!(encoded["error"]["code"], -32700);

    // Unknown method.
    let frame = json!({ "jsonrpc": "2.0", "id": 1, "method": "nope", "params": {} }).to_string();
    let encoded = serde_json::to_value(server.handle_line(&frame).await.unwrap()).unwrap();
    assert_eq!(encoded["error"]["code"], -32601);

    // A tool-level failure comes back as tool content, not as a protocol error, so the host can
    // read it and decide rather than treating the server as broken.
    let missing = tool(&server, 2, "jev_browser_start", json!({ "goal": "no url" })).await;
    assert_eq!(missing["error"]["code"], "invalid_argument");

    let unknown = tool(&server, 3, "jev_browser_run", json!({ "session_id": "ses_nope" })).await;
    assert_eq!(unknown["error"]["code"], "unknown_session");

    // The server still works afterwards.
    let result = call(&server, 4, "tools/list", json!({})).await;
    assert_eq!(result["tools"].as_array().unwrap().len(), 7);
}

#[tokio::test]
async fn a_consequential_action_is_gated_over_mcp() {
    let server = server(
        CHECKOUT,
        vec![
            Planned::targeting(Operation::Click, "Continue to checkout"),
            Planned::targeting(Operation::TypeText, "Name on card"),
            Planned::targeting(Operation::Click, "Place order"),
            Planned::targeting(Operation::Click, "Place order"),
            Planned::new(Operation::Done),
        ],
    );
    let started = tool(
        &server,
        1,
        "jev_browser_start",
        json!({
            "url": "https://fixture.test/shop/cart",
            "goal": "Buy the item in the cart",
            "context": { "name on card": "Ada Lovelace" }
        }),
    )
    .await;
    let session_id = started["session_id"].as_str().unwrap().to_string();

    let gated = tool(&server, 2, "jev_browser_run", json!({ "session_id": session_id })).await;
    assert_eq!(gated["status"], "needs_confirmation");
    assert_eq!(gated["consequence"], "places an order or booking");
    assert!(gated["next"].as_str().unwrap().contains("Ask the user"));
    let request_id = gated["request_id"].as_str().unwrap().to_string();

    let approved = tool(
        &server,
        3,
        "jev_browser_confirm",
        json!({ "session_id": session_id, "request_id": request_id, "approved": true }),
    )
    .await;
    assert_eq!(approved["approved"], true);

    let done = tool(&server, 4, "jev_browser_run", json!({ "session_id": session_id })).await;
    assert_eq!(done["status"], "done", "{done}");
    assert!(done["url"].as_str().unwrap().ends_with("/confirmed"));
}
