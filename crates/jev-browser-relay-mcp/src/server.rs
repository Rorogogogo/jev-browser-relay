//! The stateful MCP server: session registry, tool dispatch, and the stdio transport.

use crate::protocol::*;
use crate::tools;
use async_trait::async_trait;
use jev_browser_relay_browser::{HarnessBackend, DEFAULT_VIEWPORT};
use jev_browser_relay_core::browser::BrowserBackend;
use jev_browser_relay_core::config::RuntimeConfig;
use jev_browser_relay_core::error::{RelayError, Result};
use jev_browser_relay_core::jev::JevTransport;
use jev_browser_relay_core::registry::{new_session_id, SessionRegistry};
use jev_browser_relay_core::session::{task_fingerprint, RunBudget, RunOutcome, Session};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{debug, info, warn};

/// How a session gets its browser. Injectable so the server can be driven against the scripted
/// backend in tests, with no Chrome anywhere.
#[async_trait]
pub trait BackendFactory: Send + Sync {
    /// `idle` says nothing else in this runtime holds a browser, which is what lets the
    /// implementation do disruptive work — upgrading the browser layer, reclaiming stray tabs —
    /// at the only moment it costs nobody anything.
    async fn create(&self, url: &str, idle: bool) -> Result<Box<dyn BrowserBackend>>;
    fn describe(&self) -> String;
}

/// A scripted page model instead of a browser. Lets an MCP client drive the full tool surface
/// with no Chrome and no daemon — used by the tests, and by `mcp --fixture` for demos.
pub struct ScriptedFactory {
    pub fixture: String,
}

#[async_trait]
impl BackendFactory for ScriptedFactory {
    async fn create(&self, _url: &str, _idle: bool) -> Result<Box<dyn BrowserBackend>> {
        Ok(Box::new(jev_browser_relay_browser::ScriptedBackend::from_json(&self.fixture)?))
    }

    fn describe(&self) -> String {
        "scripted".into()
    }
}

/// The real thing: a background tab in the user's Chrome, via the Browser Harness daemon.
pub struct HarnessFactory {
    pub viewport: (u32, u32),
}

impl Default for HarnessFactory {
    fn default() -> Self {
        Self { viewport: DEFAULT_VIEWPORT }
    }
}

#[async_trait]
impl BackendFactory for HarnessFactory {
    async fn create(&self, url: &str, idle: bool) -> Result<Box<dyn BrowserBackend>> {
        Ok(Box::new(HarnessBackend::connect_with(url, self.viewport, idle).await?))
    }

    fn describe(&self) -> String {
        "browser-harness".into()
    }
}

pub struct RelayServer {
    registry: SessionRegistry,
    jev: Arc<dyn JevTransport>,
    factory: Arc<dyn BackendFactory>,
    config: RuntimeConfig,
}

impl RelayServer {
    pub fn new(jev: Arc<dyn JevTransport>, factory: Arc<dyn BackendFactory>, config: RuntimeConfig) -> Self {
        Self { registry: SessionRegistry::new(), jev, factory, config }
    }

    /// Serve MCP over stdio until the client closes the stream.
    pub async fn serve_stdio(&self) -> std::io::Result<()> {
        let stdin = BufReader::new(tokio::io::stdin());
        let mut stdout = tokio::io::stdout();
        let mut lines = stdin.lines();

        info!(
            jev = %self.jev.describe(),
            browser = %self.factory.describe(),
            "jev-browser-relay MCP server ready on stdio"
        );

        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            let Some(response) = self.handle_line(&line).await else {
                continue; // a notification
            };
            let mut encoded = serde_json::to_string(&response).unwrap_or_else(|e| {
                let fallback =
                    Response::error(Value::Null, INTERNAL_ERROR, format!("could not encode response: {e}"));
                serde_json::to_string(&fallback).expect("fallback always encodes")
            });
            encoded.push('\n');
            stdout.write_all(encoded.as_bytes()).await?;
            stdout.flush().await?;
        }

        info!("client disconnected; closing sessions");
        self.shutdown().await;
        Ok(())
    }

    /// Close every live session so no browser tab is orphaned.
    pub async fn shutdown(&self) {
        for id in self.registry.ids().await {
            if let Ok(handle) = self.registry.get(&id).await {
                let _ = handle.lock().await.stop().await;
            }
        }
    }

    /// Handle one JSON-RPC frame. Returns `None` for a notification, which takes no response.
    ///
    /// Public because it is the whole server minus the transport: an HTTP transport, or a test,
    /// drives exactly this.
    pub async fn handle_line(&self, line: &str) -> Option<Response> {
        let request: Request = match serde_json::from_str(line) {
            Ok(request) => request,
            Err(error) => {
                warn!(error = %error, "malformed JSON-RPC frame");
                return Some(Response::error(Value::Null, PARSE_ERROR, format!("invalid JSON: {error}")));
            }
        };

        if request.is_notification() {
            debug!(method = %request.method, "notification");
            return None;
        }
        let id = request.id.clone().unwrap_or(Value::Null);
        if !request.jsonrpc.is_empty() && request.jsonrpc != "2.0" {
            return Some(Response::error(id, INVALID_REQUEST, "only JSON-RPC 2.0 is supported"));
        }

        Some(self.dispatch(id, &request.method, request.params).await)
    }

    async fn dispatch(&self, id: Value, method: &str, params: Value) -> Response {
        match method {
            "initialize" => {
                let version = negotiate_version(params.get("protocolVersion").and_then(Value::as_str));
                Response::ok(
                    id,
                    json!({
                        "protocolVersion": version,
                        "capabilities": { "tools": { "listChanged": false } },
                        "serverInfo": { "name": "jev-browser-relay", "version": env!("CARGO_PKG_VERSION") },
                        "instructions": INSTRUCTIONS,
                    }),
                )
            }
            "ping" => Response::ok(id, json!({})),
            "tools/list" => Response::ok(id, json!({ "tools": tools::definitions() })),
            "tools/call" => self.call_tool(id, params).await,
            other => Response::error(id, METHOD_NOT_FOUND, format!("unknown method {other}")),
        }
    }

    async fn call_tool(&self, id: Value, params: Value) -> Response {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return Response::error(id, INVALID_PARAMS, "tools/call requires a name");
        };
        let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));

        match self.run_tool(name, &arguments).await {
            Ok(result) => Response::ok(id, tool_result(&result, false)),
            Err(error) => {
                // A tool-level failure is reported as tool content, not as a protocol error:
                // the host should see it and decide, not treat the server as broken.
                warn!(tool = name, error = %error, "tool call failed");
                Response::ok(
                    id,
                    tool_result(
                        &json!({ "error": { "code": error.code(), "message": error.to_string() } }),
                        true,
                    ),
                )
            }
        }
    }

    async fn run_tool(&self, name: &str, arguments: &Value) -> Result<Value> {
        match name {
            tools::START => self.start(arguments).await,
            tools::RUN => self.run(arguments).await,
            tools::PROVIDE_INPUT => self.provide_input(arguments).await,
            tools::PROVIDE_REASONING => self.provide_reasoning(arguments).await,
            tools::CONFIRM => self.confirm(arguments).await,
            tools::OBSERVE => self.observe(arguments).await,
            tools::STOP => self.stop(arguments).await,
            other => Err(RelayError::InvalidArgument(format!("unknown tool {other}"))),
        }
    }

    // --- tools ---

    async fn start(&self, arguments: &Value) -> Result<Value> {
        let url = required_str(arguments, "url")?;
        let goal = required_str(arguments, "goal")?;
        let context = arguments.get("context").cloned().filter(|c| c.is_object());
        let force_new = arguments.get("force_new").and_then(Value::as_bool).unwrap_or(false);

        // Starting a task this runtime is already working on is nearly always an agent that
        // forgot it had a session — the paused-then-restarted pattern. Handing back the live
        // session resumes the work instead of abandoning it behind a second Chrome tab.
        if !force_new {
            let fingerprint = task_fingerprint(&url, &goal);
            if let Some((existing_id, handle)) = self.registry.find_live_by_fingerprint(&fingerprint).await {
                let session = handle.lock().await;
                info!(session = %existing_id, "reusing the live session for this task");
                return Ok(json!({
                    "session_id": existing_id,
                    "status": session.status.as_str(),
                    "url": session.current_url(),
                    "reused": true,
                    "next": "This task was already open, so it was resumed rather than restarted. \
                             Answer any pending request, then call jev_browser_run. \
                             Pass force_new if you truly need a second independent session."
                }));
            }
        }

        // Only while nothing else is running may provisioning do disruptive work.
        let idle = self.registry.is_idle().await;
        let backend = self.factory.create(&url, idle).await?;
        let id = new_session_id();
        let session =
            Session::start(id.clone(), goal, context, self.jev.clone(), backend, self.config.clone()).await?;
        let url = session.current_url().to_string();
        let known = session.value_pool.assertable().iter().map(|e| e.key.clone()).collect::<Vec<_>>();
        self.registry.insert(session).await;

        Ok(json!({
            "session_id": id,
            "status": "ready",
            "url": url,
            "reused": false,
            "context_keys": known,
            "next": "Call jev_browser_run. One call will execute many browser actions."
        }))
    }

    async fn run(&self, arguments: &Value) -> Result<Value> {
        let id = required_str(arguments, "session_id")?;
        let handle = self.registry.get(&id).await?;
        let mut session = handle.lock().await;

        let budget = RunBudget {
            max_steps: arguments
                .get("max_steps")
                .and_then(Value::as_u64)
                .map(|v| v.clamp(1, 500) as u32)
                .unwrap_or(self.config.default_max_steps),
            max_duration_ms: arguments
                .get("max_duration_ms")
                .and_then(Value::as_u64)
                .map(|v| v.clamp(100, 600_000))
                .unwrap_or(self.config.default_max_duration_ms),
        };

        let outcome = session.run(budget).await;
        let mut result = serde_json::to_value(&outcome)
            .map_err(|e| RelayError::Browser(format!("could not encode outcome: {e}")))?;
        if let Some(map) = result.as_object_mut() {
            map.insert("session_id".into(), json!(id));
            map.insert("url".into(), json!(session.current_url()));
            map.insert("metrics".into(), session.metrics_report());
            map.insert("next".into(), json!(next_step_hint(&outcome)));
        }
        Ok(result)
    }

    async fn provide_input(&self, arguments: &Value) -> Result<Value> {
        let id = required_str(arguments, "session_id")?;
        let request_id = required_str(arguments, "request_id")?;
        let value = required_str(arguments, "value")?;
        let remember = arguments.get("remember").and_then(Value::as_bool).unwrap_or(true);

        let handle = self.registry.get(&id).await?;
        let mut session = handle.lock().await;
        session.provide_input(&request_id, &value, remember)?;
        Ok(json!({
            "session_id": id,
            "status": session.status.as_str(),
            "next": "Call jev_browser_run to continue from where the task paused."
        }))
    }

    async fn provide_reasoning(&self, arguments: &Value) -> Result<Value> {
        let id = required_str(arguments, "session_id")?;
        let request_id = required_str(arguments, "request_id")?;
        let guidance = required_str(arguments, "guidance")?;

        let handle = self.registry.get(&id).await?;
        let mut session = handle.lock().await;
        session.provide_reasoning(&request_id, &guidance)?;
        Ok(json!({
            "session_id": id,
            "status": session.status.as_str(),
            "next": "Call jev_browser_run. Your guidance steers the next decision only."
        }))
    }

    async fn confirm(&self, arguments: &Value) -> Result<Value> {
        let id = required_str(arguments, "session_id")?;
        let request_id = required_str(arguments, "request_id")?;
        let approved = arguments
            .get("approved")
            .and_then(Value::as_bool)
            .ok_or_else(|| RelayError::InvalidArgument("approved must be a boolean".into()))?;

        let handle = self.registry.get(&id).await?;
        let mut session = handle.lock().await;
        session.confirm(&request_id, approved)?;
        Ok(json!({
            "session_id": id,
            "status": session.status.as_str(),
            "approved": approved,
            "next": if approved {
                "Call jev_browser_run to execute the approved action and continue."
            } else {
                "The session is blocked. Call jev_browser_stop for the final report."
            }
        }))
    }

    async fn observe(&self, arguments: &Value) -> Result<Value> {
        let id = required_str(arguments, "session_id")?;
        let screenshot = arguments.get("screenshot").and_then(Value::as_bool).unwrap_or(false);
        let handle = self.registry.get(&id).await?;
        let mut session = handle.lock().await;
        session.observe_now(screenshot).await
    }

    async fn stop(&self, arguments: &Value) -> Result<Value> {
        let id = required_str(arguments, "session_id")?;
        let handle = self.registry.remove(&id).await?;
        let mut session = handle.lock().await;
        let metrics = session.stop().await?;
        let verification = session.verification().and_then(|v| serde_json::to_value(v).ok());
        info!(session = %id, "session stopped");
        Ok(json!({
            "session_id": id,
            "status": session.status.as_str(),
            "final_url": session.current_url(),
            "verification": verification,
            "metrics": metrics,
        }))
    }
}

/// Wrap a value as MCP tool content. The JSON is also sent as plain text, because that is what
/// every MCP client can read today.
fn tool_result(value: &Value, is_error: bool) -> Value {
    let text = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": value,
        "isError": is_error,
    })
}

fn required_str(arguments: &Value, key: &str) -> Result<String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| {
            RelayError::InvalidArgument(format!("{key} is required and must be a non-empty string"))
        })
}

fn next_step_hint(outcome: &RunOutcome) -> &'static str {
    match outcome {
        RunOutcome::Done { .. } => {
            "Read `verification`. If host_verification_required is true, check the page yourself with \
             jev_browser_observe before reporting success. Then call jev_browser_stop."
        }
        RunOutcome::NeedsInput { .. } => "Call jev_browser_provide_input with the request_id and the value.",
        RunOutcome::NeedsReasoning { .. } => {
            "Call jev_browser_provide_reasoning with the request_id and one or two sentences of guidance."
        }
        RunOutcome::NeedsConfirmation { .. } => {
            "Ask the user if this was not already authorised, then call jev_browser_confirm."
        }
        RunOutcome::Blocked { .. } => {
            "The task cannot proceed. Call jev_browser_observe to see why, or stop."
        }
        RunOutcome::BudgetExceeded { .. } => "Call jev_browser_run again to continue; nothing is lost.",
        RunOutcome::Error { .. } => {
            "Read the error code. Most are recoverable by calling jev_browser_run again."
        }
    }
}

const INSTRUCTIONS: &str = "\
jev-browser-relay runs browser tasks with a fast policy model in the loop, so you are not.

Work like this:
1. jev_browser_start — give the goal AND a `context` object with every value the task implies.
   Values you supply up front are round trips you will not be asked to make later.
2. jev_browser_run — one call executes many actions. Do not call it once per click.
3. Answer only what it asks for: provide_input, provide_reasoning, or confirm. Keep the same
   session_id; the task resumes where it paused.
4. Check `verification` on done. If host_verification_required is true, look at the page before
   reporting success.
5. jev_browser_stop for the final report.";
