//! [`BrowserBackend`] over the Browser Harness daemon.
//!
//! Owns one background target for the session, so the user's visible Chrome tab is never hijacked
//! and one CDP session is reused for every call. Focus emulation keeps the background tab
//! rendering, otherwise Chrome throttles animations and menus never open.

use async_trait::async_trait;
use jev_browser_relay_core::browser::BrowserBackend;
use jev_browser_relay_core::error::{RelayError, Result};
use jev_browser_relay_core::snapshot::{ActionKind, RawAction, Snapshot};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use tracing::{debug, warn};

use crate::harness::HarnessIpc;

pub const SNAPSHOT_JS: &str = include_str!("js/snapshot.js");
pub const EXECUTE_JS: &str = include_str!("js/execute.js");
pub const SETTLE_JS: &str = include_str!("js/settle.js");

/// Expression that returns only the freshness marker — far cheaper than a full snapshot, and it
/// is what the loop calls before every action.
fn marker_expression() -> String {
    format!("(() => {{ const state = {SNAPSHOT_JS}; return state?.marker ?? null; }})()")
}

pub struct HarnessBackend {
    ipc: HarnessIpc,
    session_id: String,
    target_id: String,
    /// The action whose settle is still owed, applied lazily at the next observation.
    pending_settle: Option<RawAction>,
    viewport: (u32, u32),
    /// Distinct URLs this tab has visited. Lets the runtime offer BACK without spending a CDP
    /// call on `Page.getNavigationHistory` at every observation.
    last_url: Option<String>,
    navigation_depth: u32,
}

impl HarnessBackend {
    /// Open a background tab at `url` and wait for it to finish loading.
    pub async fn connect(url: &str, viewport: (u32, u32)) -> Result<Self> {
        let ipc = HarnessIpc::new();
        if !ipc.is_available().await {
            return Err(RelayError::BrowserDisconnected(
                "the Browser Harness daemon is not running. Start it with `browser-harness --doctor`, \
                 then re-run. See `jev-browser-relay doctor`."
                    .into(),
            ));
        }

        let created =
            ipc.cdp("Target.createTarget", None, json!({ "url": "about:blank", "background": true })).await?;
        let target_id = created
            .get("targetId")
            .and_then(Value::as_str)
            .ok_or_else(|| RelayError::Browser("Target.createTarget returned no targetId".into()))?
            .to_string();

        let attached =
            ipc.cdp("Target.attachToTarget", None, json!({ "targetId": target_id, "flatten": true })).await?;
        let session_id = attached
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| RelayError::Browser("Target.attachToTarget returned no sessionId".into()))?
            .to_string();

        let backend = Self {
            ipc,
            session_id,
            target_id,
            pending_settle: None,
            viewport,
            last_url: None,
            navigation_depth: 0,
        };

        backend
            .call(
                "Emulation.setDeviceMetricsOverride",
                json!({ "width": viewport.0, "height": viewport.1, "deviceScaleFactor": 1, "mobile": false }),
            )
            .await?;
        // Without this, a background tab throttles rAF and dropdowns never paint.
        backend.call("Emulation.setFocusEmulationEnabled", json!({ "enabled": true })).await?;

        backend.navigate(url).await?;
        Ok(backend)
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.ipc.cdp(method, Some(&self.session_id), params).await
    }

    async fn evaluate(&self, expression: &str, await_promise: bool) -> Result<Value> {
        self.ipc.evaluate(Some(&self.session_id), expression, await_promise).await
    }

    async fn navigate(&self, url: &str) -> Result<()> {
        self.call("Page.navigate", json!({ "url": url }))
            .await
            .map_err(|e| RelayError::Navigation(format!("could not navigate to {url}: {e}")))?;
        // Poll readiness rather than sleeping a fixed amount.
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if let Ok(Value::String(state)) = self.evaluate("document.readyState", false).await {
                if state == "complete" || state == "interactive" {
                    return Ok(());
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Err(RelayError::Navigation(format!("{url} did not finish loading within 20s")))
    }

    /// Apply the settle owed from the previous action. Read-only, and already logged, so a
    /// navigation interrupting it is harmless.
    async fn drain_settle(&mut self) -> u64 {
        let Some(action) = self.pending_settle.take() else { return 0 };
        let Ok(payload) = serde_json::to_string(&action) else { return 0 };
        let started = Instant::now();
        if let Err(error) = self.evaluate(&format!("{SETTLE_JS}({payload})"), true).await {
            debug!(error = %error, "settle interrupted, continuing");
        }
        started.elapsed().as_millis() as u64
    }
}

#[async_trait]
impl BrowserBackend for HarnessBackend {
    async fn observe(&mut self) -> Result<Snapshot> {
        self.drain_settle().await;
        // A snapshot taken mid-navigation is useless; retry briefly rather than returning one.
        let mut last = RelayError::StalePage("page did not settle".into());
        for attempt in 0..10 {
            match self.evaluate(SNAPSHOT_JS, false).await {
                Ok(Value::Null) => last = RelayError::StalePage("document is navigating".into()),
                Ok(value) => {
                    let mut snapshot = serde_json::from_value::<Snapshot>(value).map_err(|e| {
                        RelayError::Browser(format!("snapshot did not match the expected shape: {e}"))
                    })?;
                    if self.last_url.as_deref().is_some_and(|previous| previous != snapshot.url) {
                        self.navigation_depth += 1;
                    }
                    self.last_url = Some(snapshot.url.clone());
                    snapshot.can_go_back = self.navigation_depth > 0;
                    return Ok(snapshot);
                }
                Err(error) if error.is_recoverable() => last = error,
                Err(error) => return Err(error),
            }
            if attempt < 9 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        Err(last)
    }

    async fn is_fresh(&mut self, snapshot: &Snapshot, action: Option<&RawAction>) -> Result<bool> {
        // For a targeted action, compare that node's own identity and state. Page-wide churn
        // (a ticking clock, a lazy image) must not force a re-decision.
        if let Some(action) = action {
            if matches!(action.kind, ActionKind::Click | ActionKind::Select) {
                let Some(node) = action.node else { return Ok(false) };
                let expression = format!(
                    "(() => {{ const c = window.__jevRelay; return c ? [c.pageKey(), c.guard(c.nodes.get({node}))] : null; }})()"
                );
                let current = match self.evaluate(&expression, false).await {
                    Ok(value) => value,
                    Err(error) if error.is_recoverable() => return Ok(false),
                    Err(error) => return Err(error),
                };
                let expected = json!([snapshot.page_key, snapshot.guards.get(&node.to_string())]);
                return Ok(current == expected);
            }
        }
        match self.evaluate(&marker_expression(), false).await {
            Ok(marker) => Ok(marker == snapshot.marker),
            Err(error) if error.is_recoverable() => Ok(false),
            Err(error) => Err(error),
        }
    }

    async fn execute(&mut self, action: &RawAction, _snapshot: &Snapshot, text: Option<&str>) -> Result<()> {
        match action.kind {
            ActionKind::Wait => {
                // A bounded tick, not a sleep-to-hope.
                tokio::time::sleep(Duration::from_millis(100)).await;
                self.pending_settle = None;
                return Ok(());
            }
            ActionKind::Scroll => {
                let delta = action.delta.unwrap_or(560);
                let (w, h) = self.viewport;
                self.call(
                    "Input.dispatchMouseEvent",
                    json!({ "type": "mouseWheel", "x": w / 2, "y": h / 2, "deltaX": 0, "deltaY": delta }),
                )
                .await?;
                self.pending_settle = None;
                return Ok(());
            }
            _ => {}
        }

        let Some(node) = action.node else {
            return Err(RelayError::Browser("action has no observed node".into()));
        };
        if action.kind == ActionKind::Fill && text.is_none() {
            return Err(RelayError::Browser("TYPE_TEXT reached the executor with no value".into()));
        }

        // Resolve geometry and re-verify *now*. The loop's freshness check happened before value
        // resolution, so this is the check that actually guards the input.
        let payload = serde_json::to_string(&json!({
            "node": node,
            "kind": action.kind,
            "value": action.value.clone().unwrap_or_default(),
        }))
        .map_err(|e| RelayError::Browser(e.to_string()))?;

        let resolved = self.evaluate(&format!("{EXECUTE_JS}({payload})"), false).await?;
        if resolved.is_null() {
            return Err(RelayError::StalePage(format!(
                "\"{}\" moved, became hidden, or is covered by another element",
                action.element_label()
            )));
        }

        if action.kind != ActionKind::Select {
            let (Some(x), Some(y)) =
                (resolved.get("x").and_then(Value::as_f64), resolved.get("y").and_then(Value::as_f64))
            else {
                return Err(RelayError::Browser("executor returned no coordinates".into()));
            };

            for event in ["mousePressed", "mouseReleased"] {
                self.call(
                    "Input.dispatchMouseEvent",
                    json!({ "type": event, "x": x, "y": y, "button": "left", "clickCount": 1 }),
                )
                .await?;
            }

            if action.kind == ActionKind::Fill {
                // Select-all then insert, so typing replaces rather than appends.
                let modifiers = if cfg!(target_os = "macos") { 4 } else { 2 };
                self.call(
                    "Input.dispatchKeyEvent",
                    json!({ "type": "keyDown", "key": "a", "code": "KeyA", "modifiers": modifiers, "commands": ["selectAll"] }),
                )
                .await?;
                self.call(
                    "Input.dispatchKeyEvent",
                    json!({ "type": "keyUp", "key": "a", "code": "KeyA", "modifiers": modifiers }),
                )
                .await?;
                // `text` is a value from the runtime's own pool, never model-authored code.
                self.call("Input.insertText", json!({ "text": text.unwrap_or_default() })).await?;
            }
        }

        self.pending_settle = Some(action.clone());
        Ok(())
    }

    async fn settle(&mut self, _action: &RawAction) -> Result<u64> {
        // Deferred to the next `observe`, so the action is recorded before we spend time waiting.
        Ok(0)
    }

    async fn back(&mut self) -> Result<()> {
        let history = self.call("Page.getNavigationHistory", json!({})).await?;
        let index = history.get("currentIndex").and_then(Value::as_i64).unwrap_or(0);
        if index <= 0 {
            return Err(RelayError::Navigation("there is no previous page".into()));
        }
        let entry = history
            .get("entries")
            .and_then(Value::as_array)
            .and_then(|entries| entries.get((index - 1) as usize))
            .and_then(|e| e.get("id"))
            .and_then(Value::as_i64)
            .ok_or_else(|| RelayError::Navigation("could not read the previous history entry".into()))?;
        self.call("Page.navigateToHistoryEntry", json!({ "entryId": entry })).await?;
        self.pending_settle = None;
        self.navigation_depth = self.navigation_depth.saturating_sub(1);
        Ok(())
    }

    async fn screenshot(&mut self) -> Result<Option<String>> {
        let shot = self.call("Page.captureScreenshot", json!({ "format": "jpeg", "quality": 72 })).await?;
        Ok(shot.get("data").and_then(Value::as_str).map(str::to_string))
    }

    fn protocol_calls(&self) -> u32 {
        self.ipc.call_count()
    }

    async fn close(&mut self) -> Result<()> {
        if self.target_id.is_empty() {
            return Ok(());
        }
        let target = std::mem::take(&mut self.target_id);
        if let Err(error) = self.ipc.cdp("Target.closeTarget", None, json!({ "targetId": target })).await {
            warn!(error = %error, "could not close the browser target");
        }
        Ok(())
    }

    fn describe(&self) -> String {
        format!("browser-harness:{}", self.session_id)
    }
}
