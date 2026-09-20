//! A native Rust client for the Browser Harness daemon.
//!
//! The harness already owns the CDP websocket to real Chrome and exposes it over a local
//! socket. That boundary is language-agnostic, so the control plane speaks it directly —
//! **no Python in the hot path**, and no second browser-automation stack.
//!
//! Wire protocol (from `browser_harness/_ipc.py`, MIT):
//!
//! - POSIX: `AF_UNIX` at `$BH_RUNTIME_DIR/bu[-NAME].sock`, mode 0600.
//! - Windows: TCP loopback, with a `token` read from the sibling `.port` file.
//! - One newline-terminated JSON request per connection, one JSON response back.
//! - `{"method","params","session_id"}` → `{"result":…}` or `{"error":…}`.
//! - `{"meta":"ping"}` → `{"pong":true,"pid":…}`; also `session`, `connection_status`.
//! - `Target.*` methods are sent with no session id; the daemon supplies its own otherwise.

use jev_browser_relay_core::error::{RelayError, Result};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
/// Screenshots routinely outrun an ordinary CDP round trip.
const SLOW_RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);

/// Where the harness keeps its runtime files, mirroring `browser_harness.paths`.
pub fn runtime_dir() -> PathBuf {
    if let Ok(raw) = std::env::var("BH_RUNTIME_DIR") {
        return PathBuf::from(shellexpand_home(&raw));
    }
    if let Ok(raw) = std::env::var("BH_TMP_DIR") {
        return PathBuf::from(shellexpand_home(&raw));
    }
    home_dir().join("runtime")
}

fn home_dir() -> PathBuf {
    for key in ["BH_HOME", "BROWSER_HARNESS_HOME"] {
        if let Ok(raw) = std::env::var(key) {
            return PathBuf::from(shellexpand_home(&raw));
        }
    }
    if let Ok(base) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(shellexpand_home(&base)).join("browser-harness");
    }
    user_home().join(".config").join("browser-harness")
}

fn user_home() -> PathBuf {
    std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).map(PathBuf::from).unwrap_or_default()
}

fn shellexpand_home(raw: &str) -> String {
    match raw.strip_prefix("~/") {
        Some(rest) => user_home().join(rest).to_string_lossy().into_owned(),
        None => raw.to_string(),
    }
}

/// `BU_NAME`, the harness instance name.
pub fn instance_name() -> String {
    std::env::var("BU_NAME").ok().filter(|n| !n.is_empty()).unwrap_or_else(|| "default".into())
}

fn runtime_stem(name: &str) -> String {
    // The harness uses the bare stem when a caller-supplied runtime dir isolates the instance.
    let isolated = std::env::var("BH_RUNTIME_DIR").is_ok() || std::env::var("BH_TMP_DIR").is_ok();
    let shared = std::env::var("BH_RUNTIME_DIR_SHARED").as_deref() == Ok("1")
        || std::env::var("BH_TMP_DIR_SHARED").as_deref() == Ok("1");
    if isolated && !shared {
        "bu".to_string()
    } else {
        format!("bu-{name}")
    }
}

pub fn socket_path() -> PathBuf {
    runtime_dir().join(format!("{}.sock", runtime_stem(&instance_name())))
}

pub fn port_file_path() -> PathBuf {
    runtime_dir().join(format!("{}.port", runtime_stem(&instance_name())))
}

/// A client for the harness daemon. Cheap to clone-by-value: each request opens its own
/// short-lived connection, exactly as the Python client does, so there is no shared stream to
/// get out of sync.
#[derive(Debug, Clone, Default)]
pub struct HarnessIpc {
    calls: std::sync::Arc<AtomicU32>,
}

impl HarnessIpc {
    pub fn new() -> Self {
        Self::default()
    }

    /// How many CDP calls this client has issued, for metrics.
    pub fn call_count(&self) -> u32 {
        self.calls.load(Ordering::Relaxed)
    }

    async fn request(&self, body: Value, timeout: Duration) -> Result<Value> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let payload = {
            // Windows TCP loopback has no filesystem permission boundary, so the daemon
            // requires a shared token on every request. On POSIX the 0600 socket is the boundary.
            #[cfg(windows)]
            let body = {
                let mut body = body;
                if let (Some(token), Some(map)) = (read_windows_token()?, body.as_object_mut()) {
                    map.insert("token".into(), json!(token));
                }
                body
            };
            let mut text = serde_json::to_string(&body)
                .map_err(|e| RelayError::Browser(format!("could not encode request: {e}")))?;
            text.push('\n');
            text
        };

        let line = self.round_trip(payload.as_bytes(), timeout).await?;
        let response: Value = serde_json::from_str(&line)
            .map_err(|e| RelayError::Browser(format!("daemon sent malformed JSON: {e}")))?;

        if let Some(error) = response.get("error") {
            let message = error.as_str().map(str::to_string).unwrap_or_else(|| error.to_string());
            // The harness reports a vanished document as an evaluation exception; that is a
            // stale page, which the loop recovers from, not a hard failure.
            if message.contains("Cannot find context")
                || message.contains("Target closed")
                || message.contains("detached")
            {
                return Err(RelayError::StalePage(message));
            }
            return Err(RelayError::Browser(message));
        }
        Ok(response)
    }

    #[cfg(unix)]
    async fn round_trip(&self, payload: &[u8], timeout: Duration) -> Result<String> {
        use tokio::net::UnixStream;
        let path = socket_path();
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(&path))
            .await
            .map_err(|_| {
                RelayError::BrowserDisconnected(format!("timed out connecting to {}", path.display()))
            })?
            .map_err(|e| {
                RelayError::BrowserDisconnected(format!(
                    "cannot reach the Browser Harness daemon at {}: {e}",
                    path.display()
                ))
            })?;
        exchange(stream, payload, timeout).await
    }

    #[cfg(windows)]
    async fn round_trip(&self, payload: &[u8], timeout: Duration) -> Result<String> {
        use tokio::net::TcpStream;
        let (port, _) = read_windows_endpoint()?;
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(("127.0.0.1", port)))
            .await
            .map_err(|_| RelayError::BrowserDisconnected("timed out connecting to the daemon".into()))?
            .map_err(|e| {
                RelayError::BrowserDisconnected(format!("cannot reach the daemon on 127.0.0.1:{port}: {e}"))
            })?;
        exchange(stream, payload, timeout).await
    }

    /// Raw CDP. `Target.*` goes without a session, matching the daemon's own routing.
    pub async fn cdp(&self, method: &str, session_id: Option<&str>, params: Value) -> Result<Value> {
        let timeout =
            if method == "Page.captureScreenshot" { SLOW_RESPONSE_TIMEOUT } else { DEFAULT_RESPONSE_TIMEOUT };
        let mut body = json!({ "method": method, "params": params });
        if !method.starts_with("Target.") {
            if let Some(session) = session_id {
                body["session_id"] = json!(session);
            }
        }
        let response = self.request(body, timeout).await?;
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }

    /// `Runtime.evaluate`, returning the value by reference-free JSON.
    pub async fn evaluate(
        &self,
        session_id: Option<&str>,
        expression: &str,
        await_promise: bool,
    ) -> Result<Value> {
        let result = self
            .cdp(
                "Runtime.evaluate",
                session_id,
                json!({ "expression": expression, "returnByValue": true, "awaitPromise": await_promise }),
            )
            .await?;
        if let Some(details) = result.get("exceptionDetails") {
            // An exception here almost always means the document went away mid-evaluation.
            let text = details.get("text").and_then(Value::as_str).unwrap_or("evaluation failed");
            return Err(RelayError::StalePage(format!("document changed during evaluation: {text}")));
        }
        Ok(result.get("result").and_then(|r| r.get("value")).cloned().unwrap_or(Value::Null))
    }

    pub async fn ping(&self) -> Result<Value> {
        self.request(json!({ "meta": "ping" }), Duration::from_secs(2)).await
    }

    pub async fn connection_status(&self) -> Result<Value> {
        self.request(json!({ "meta": "connection_status" }), Duration::from_secs(5)).await
    }

    pub async fn daemon_session(&self) -> Result<Option<String>> {
        let response = self.request(json!({ "meta": "session" }), Duration::from_secs(5)).await?;
        Ok(response.get("session_id").and_then(Value::as_str).map(str::to_string))
    }

    pub async fn is_available(&self) -> bool {
        matches!(self.ping().await, Ok(response) if response.get("pong") == Some(&json!(true)))
    }
}

async fn exchange<S>(mut stream: S, payload: &[u8], timeout: Duration) -> Result<String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt as _;
    tokio::time::timeout(timeout, async {
        stream.write_all(payload).await?;
        stream.flush().await?;
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        Ok::<_, std::io::Error>(line)
    })
    .await
    .map_err(|_| RelayError::BrowserDisconnected(format!("daemon did not answer within {timeout:?}")))?
    .map_err(|e| RelayError::BrowserDisconnected(format!("daemon connection failed: {e}")))
    .and_then(|line| {
        if line.trim().is_empty() {
            Err(RelayError::BrowserDisconnected("daemon closed the connection without replying".into()))
        } else {
            Ok(line)
        }
    })
}

#[cfg(windows)]
fn read_windows_endpoint() -> Result<(u16, String)> {
    let path = port_file_path();
    let text = std::fs::read_to_string(&path).map_err(|e| {
        RelayError::BrowserDisconnected(format!("cannot read the daemon port file {}: {e}", path.display()))
    })?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|e| RelayError::BrowserDisconnected(format!("malformed port file: {e}")))?;
    let port = value
        .get("port")
        .and_then(Value::as_u64)
        .ok_or_else(|| RelayError::BrowserDisconnected("port file has no port".into()))?
        as u16;
    let token = value.get("token").and_then(Value::as_str).unwrap_or_default().to_string();
    Ok((port, token))
}

#[cfg(windows)]
fn read_windows_token() -> Result<Option<String>> {
    Ok(Some(read_windows_endpoint()?.1))
}
