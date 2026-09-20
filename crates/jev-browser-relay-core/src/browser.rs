//! The browser adapter boundary.
//!
//! Everything below this trait — Chrome, CDP, the harness daemon, the in-page snapshot script —
//! stays outside the control plane. The loop is written once against this interface, so the
//! scripted test backend and the real harness backend exercise identical policy code.

use crate::error::Result;
use crate::snapshot::{RawAction, Snapshot};
use async_trait::async_trait;

/// What the loop needs a browser to do. Nothing more.
#[async_trait]
pub trait BrowserBackend: Send {
    /// One atomic observation. Implementations must not return a half-navigated page.
    async fn observe(&mut self) -> Result<Snapshot>;

    /// Is `snapshot` still an accurate description of the live page?
    ///
    /// When `action` is given, the check narrows to that element's identity and state, which
    /// tolerates harmless churn elsewhere on the page.
    async fn is_fresh(&mut self, snapshot: &Snapshot, action: Option<&RawAction>) -> Result<bool>;

    /// Execute one observed action. Implementations re-verify freshness and hit-testing
    /// immediately before dispatching input — the loop's check is necessary but not sufficient,
    /// because text resolution can happen in between.
    async fn execute(&mut self, action: &RawAction, snapshot: &Snapshot, text: Option<&str>) -> Result<()>;

    /// Short adaptive wait for the page to settle after `action`. Never a long fixed sleep.
    /// Returns how long it actually waited.
    async fn settle(&mut self, action: &RawAction) -> Result<u64>;

    /// Navigate back. Separate from `execute` because it is a browser-level operation.
    async fn back(&mut self) -> Result<()>;

    /// Optional screenshot, for debugging and host fallback only.
    async fn screenshot(&mut self) -> Result<Option<String>> {
        Ok(None)
    }

    /// CDP calls issued so far, for metrics. Backends that cannot count return 0.
    fn protocol_calls(&self) -> u32 {
        0
    }

    async fn close(&mut self) -> Result<()>;

    fn describe(&self) -> String {
        "browser".to_string()
    }
}
