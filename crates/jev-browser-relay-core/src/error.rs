//! Structured errors. Every variant carries a stable `code` for the MCP surface.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum RelayError {
    #[error("jev transport failed: {0}")]
    JevTransport(String),

    #[error("jev returned an invalid response: {0}")]
    JevInvalidResponse(String),

    #[error("jev request timed out after {0} ms")]
    JevTimeout(u64),

    #[error("jev rate limited (http {status}){}", .retry_after_ms.map(|m| format!(", retry after {m} ms")).unwrap_or_default())]
    JevRateLimited { status: u16, retry_after_ms: Option<u64> },

    #[error("browser backend failed: {0}")]
    Browser(String),

    #[error("browser disconnected: {0}")]
    BrowserDisconnected(String),

    #[error("navigation failed: {0}")]
    Navigation(String),

    /// The observed page no longer matches the decision. Recoverable: re-observe and re-decide.
    #[error("page changed since observation: {0}")]
    StalePage(String),

    #[error("no session {0}")]
    UnknownSession(String),

    #[error("no pending request {0}")]
    UnknownRequest(String),

    #[error("session {session} is {status}, which does not accept {attempted}")]
    WrongState { session: String, status: String, attempted: String },

    #[error("step budget exceeded: {used}/{limit}")]
    StepBudget { used: u32, limit: u32 },

    #[error("time budget exceeded: {used_ms}/{limit_ms} ms")]
    TimeBudget { used_ms: u64, limit_ms: u64 },

    #[error("retry budget exhausted: {0}")]
    RetryBudget(String),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("invalid argument: {0}")]
    InvalidArgument(String),
}

impl RelayError {
    /// Stable machine-readable code. Hosts branch on this, never on the message.
    pub fn code(&self) -> &'static str {
        match self {
            Self::JevTransport(_) => "jev_transport",
            Self::JevInvalidResponse(_) => "jev_invalid_response",
            Self::JevTimeout(_) => "jev_timeout",
            Self::JevRateLimited { .. } => "jev_rate_limited",
            Self::Browser(_) => "browser_error",
            Self::BrowserDisconnected(_) => "browser_disconnected",
            Self::Navigation(_) => "navigation_failed",
            Self::StalePage(_) => "stale_page",
            Self::UnknownSession(_) => "unknown_session",
            Self::UnknownRequest(_) => "unknown_request",
            Self::WrongState { .. } => "wrong_state",
            Self::StepBudget { .. } => "step_budget_exceeded",
            Self::TimeBudget { .. } => "time_budget_exceeded",
            Self::RetryBudget(_) => "retry_budget_exhausted",
            Self::Config(_) => "config_error",
            Self::InvalidArgument(_) => "invalid_argument",
        }
    }

    /// Whether the loop may recover on its own rather than surfacing to the host.
    pub fn is_recoverable(&self) -> bool {
        matches!(self, Self::StalePage(_) | Self::JevRateLimited { .. } | Self::JevTimeout(_))
    }
}

pub type Result<T> = std::result::Result<T, RelayError>;
