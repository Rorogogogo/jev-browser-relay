//! # jev-browser-relay-core
//!
//! The control plane. Jev drives the browser loop; the host model plans, fills gaps, and
//! verifies. This crate holds the session state machine, the bounded choice space handed to the
//! policy model, deterministic value resolution, the safety gate, DONE verification, and the
//! metrics that show how many host round trips were avoided.
//!
//! Both external dependencies are traits — [`jev::JevTransport`] and
//! [`browser::BrowserBackend`] — so the entire policy is exercised in tests with no network,
//! no Chrome, and no API key.

pub mod action_space;
pub mod browser;
pub mod config;
pub mod error;
pub mod history;
pub mod jev;
pub mod metrics;
pub mod registry;
pub mod safety;
pub mod session;
pub mod snapshot;
pub mod testing;
pub mod value_pool;
pub mod verification;

pub use action_space::{ActionSpace, Element};
pub use browser::BrowserBackend;
pub use config::RuntimeConfig;
pub use error::{RelayError, Result};
pub use jev::{Decision, JevRawResponse, JevRequest, JevTransport};
pub use metrics::Metrics;
pub use registry::{new_session_id, SessionRegistry};
pub use session::{RunBudget, RunOutcome, Session, SessionStatus};
pub use snapshot::{ActionKind, FieldDescription, Operation, RawAction, Snapshot};
pub use value_pool::{ValuePool, ValueSource};
pub use verification::{Verdict, VerificationReport};
