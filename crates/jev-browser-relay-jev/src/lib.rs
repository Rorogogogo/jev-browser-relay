//! TypeSafe Jev transport.
//!
//! `TYPESAFE_API_KEY` is the **only** model API key this project requires. There is
//! deliberately no OpenAI, Anthropic, OpenRouter, Mercury or Gemini path: text values come from
//! the runtime's value pool, and anything it cannot resolve goes to the host agent over MCP
//! using the session the user already pays for.

use jev_browser_relay_core::error::{RelayError, Result};
use jev_browser_relay_core::jev::{JevRawResponse, JevRequest, JevTransport};
use std::time::Duration;
use tracing::debug;

pub const DEFAULT_ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";
pub const API_KEY_ENV: &str = "TYPESAFE_API_KEY";

pub struct TypeSafeClient {
    client: reqwest::Client,
    endpoint: String,
    api_key: String,
    max_attempts: u32,
    timeout_ms: u64,
}

impl TypeSafeClient {
    /// Build from the environment. Fails loudly when the key is absent rather than silently
    /// degrading, because a missing key means no policy model at all.
    pub fn from_env(timeout_ms: u64) -> Result<Self> {
        let api_key = std::env::var(API_KEY_ENV)
            .ok()
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
            .ok_or_else(|| {
                RelayError::Config(format!(
                    "{API_KEY_ENV} is not set. It is the only model API key jev-browser-relay needs; \
                     get one at https://typesafe.ai and export it before starting the runtime."
                ))
            })?;
        let endpoint = std::env::var("TYPESAFE_ENDPOINT")
            .ok()
            .filter(|e| !e.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string());
        Self::new(api_key, endpoint, timeout_ms)
    }

    pub fn new(api_key: String, endpoint: String, timeout_ms: u64) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(timeout_ms))
            // One pooled HTTP/2 connection across the whole session: the policy loop makes one
            // request per decision, so connection reuse is most of the latency budget.
            .pool_idle_timeout(Duration::from_secs(90))
            .user_agent(concat!("jev-browser-relay/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| RelayError::Config(format!("could not build HTTP client: {e}")))?;
        Ok(Self { client, endpoint, api_key, max_attempts: 3, timeout_ms })
    }

    /// Is a key present? Used by `doctor`, which must never print the key itself.
    pub fn key_present() -> bool {
        std::env::var(API_KEY_ENV).is_ok_and(|k| !k.trim().is_empty())
    }
}

#[async_trait::async_trait]
impl JevTransport for TypeSafeClient {
    async fn post(&self, request: &JevRequest) -> Result<JevRawResponse> {
        let mut last_error: Option<RelayError> = None;

        for attempt in 0..self.max_attempts {
            let response =
                self.client.post(&self.endpoint).bearer_auth(&self.api_key).json(request).send().await;

            let response = match response {
                Ok(response) => response,
                Err(error) if error.is_timeout() => {
                    last_error = Some(RelayError::JevTimeout(self.timeout_ms));
                    break;
                }
                Err(error) => {
                    // The request never reached the model, so nothing executed. Retrying is safe.
                    last_error = Some(RelayError::JevTransport(error.to_string()));
                    if attempt + 1 < self.max_attempts {
                        backoff(attempt).await;
                        continue;
                    }
                    break;
                }
            };

            let status = response.status();
            if matches!(status.as_u16(), 429 | 503 | 529) {
                let retry_after_ms = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .map(|s| s * 1000);
                last_error = Some(RelayError::JevRateLimited { status: status.as_u16(), retry_after_ms });
                if attempt + 1 < self.max_attempts {
                    match retry_after_ms {
                        Some(ms) => tokio::time::sleep(Duration::from_millis(ms.min(5_000))).await,
                        None => backoff(attempt).await,
                    }
                    continue;
                }
                break;
            }

            if !status.is_success() {
                // The body may echo request content, so it is summarised, never logged whole.
                let body = response.text().await.unwrap_or_default();
                let detail: String = body.chars().take(200).collect();
                return Err(RelayError::JevTransport(format!("HTTP {status}: {detail}")));
            }

            return response
                .json::<JevRawResponse>()
                .await
                .map_err(|e| RelayError::JevInvalidResponse(format!("could not parse response body: {e}")));
        }

        let error = last_error.unwrap_or_else(|| RelayError::JevTransport("no attempt succeeded".into()));
        debug!(error = %error, "jev request failed after retries");
        Err(error)
    }

    fn describe(&self) -> String {
        format!("typesafe:{}", self.endpoint)
    }
}

/// Exponential backoff: 250 ms, 500 ms, 1 s.
async fn backoff(attempt: u32) {
    tokio::time::sleep(Duration::from_millis(250 * 2u64.pow(attempt.min(3)))).await;
}
